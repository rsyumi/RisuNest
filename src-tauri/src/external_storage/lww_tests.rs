use super::{
    contract::*,
    fake::{self, FakeProvider},
    lww_engine::{Admission, ExternalLwwEngine},
    lww_segment,
};
use crate::persistent_store::{
    lww::{MessageLocator, UnitMutation},
    ConversationMutation, PersistentStore, WorkingSetCommit,
};
use risunest_sync_wire::{stamp::DecimalU64, unit::UnitKey};
use futures::FutureExt;
use std::sync::Arc;

mod format_levels;

fn run<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}
pub(crate) fn repository_id_cases() -> [String; 4] {
    let [boundary, long] = representative_repository_ids();
    [fake::repository().repository_id, boundary, "x".repeat(129), long]
}

pub(crate) fn representative_repository_ids() -> [String; 2] {
    ["x".repeat(128), format!("onedrive|{}", "synthetic-provider-identity/".repeat(10))]
}

pub(crate) struct HeldAssetTransfer {
    pub inner: Arc<FakeProvider>,
    pub entered: tokio::sync::Notify,
    pub resume: tokio::sync::Notify,
    pub lose_pack_response: std::sync::atomic::AtomicBool,
    pub cancel_after_pack_loss: std::sync::atomic::AtomicBool,
    pub held: std::sync::atomic::AtomicBool,
    pub fail_pack_after: std::sync::atomic::AtomicUsize,
    pub fail_segment_begin: std::sync::atomic::AtomicBool,
    pub attempts: std::sync::Mutex<Vec<ObjectIntent>>,
    pub before_lease: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
impl HeldAssetTransfer {
    pub(crate) fn new(inner: Arc<FakeProvider>) -> Arc<Self> {
        Arc::new(Self { inner, entered: Default::default(), resume: Default::default(),
            lose_pack_response: false.into(), cancel_after_pack_loss: false.into(), held: true.into(), fail_pack_after: 0.into(), fail_segment_begin: false.into(), attempts: Default::default(), before_lease: Default::default() })
    }
}
impl Provider for HeldAssetTransfer {
    fn open_repository<'a>(&'a self, config: &'a ConnectionConfig, secret: &'a SecretRef, mode: OpenMode, cancel: &'a Cancellation)
        -> ProviderFuture<'a, (RepositoryHandle, super::capabilities::Capabilities)> { self.inner.open_repository(config, secret, mode, cancel) }
    fn read_object<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, unchanged: Option<&'a VersionToken>, sink: &'a mut dyn TransferSink, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ReadReceipt> { self.inner.read_object(repository, locator, unchanged, sink, cancel) }
    fn begin_upload<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, cancel: &'a Cancellation)
        -> ProviderFuture<'a, Option<ResumeState>> {
        Box::pin(async move {
            if intent.role==ObjectRole::Segment && self.fail_segment_begin.swap(false,std::sync::atomic::Ordering::SeqCst) {
                return Err(ProviderError::new(ErrorKind::Transient));
            }
            self.inner.begin_upload(repository,intent,cancel).await
        })
    }
    fn create_object<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, source: &'a dyn TransferSource, resume: Option<&'a ResumeState>, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ObjectReceipt> {
        Box::pin(async move {
            if intent.role == ObjectRole::Lease {
                let action = self.before_lease.lock().unwrap().take();
                if let Some(action) = action { action(); }
            }
            let mut lost = false;
            if intent.role == ObjectRole::Pack {
                let attempted = { let mut attempts = self.attempts.lock().unwrap(); attempts.push(intent.clone()); attempts.len() };
                let fail_after = self.fail_pack_after.load(std::sync::atomic::Ordering::SeqCst);
                if fail_after > 0 && attempted >= fail_after { return Err(ProviderError::new(ErrorKind::RateLimited)); }
                if self.held.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    self.entered.notify_one(); self.resume.notified().await;
                }
                lost = self.lose_pack_response.swap(false, std::sync::atomic::Ordering::SeqCst);
                if lost {
                    self.inner.state.lock().unwrap().lose_response = true;
                }
            }
            let result = self.inner.create_object(repository, intent, source, resume, cancel).await;
            if lost && self.cancel_after_pack_loss.load(std::sync::atomic::Ordering::SeqCst) { cancel.cancel(); }
            result
        })
    }
    fn compare_exchange_head<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, expected: &'a ExpectedHead, head: &'a HeadBytes, cancel: &'a Cancellation)
        -> ProviderFuture<'a, HeadReceipt> { self.inner.compare_exchange_head(repository, locator, expected, head, cancel) }
    fn replace_head<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, head: &'a HeadBytes, cancel: &'a Cancellation)
        -> ProviderFuture<'a, HeadReceipt> { self.inner.replace_head(repository, locator, head, cancel) }
    fn list_objects<'a>(&'a self, repository: &'a RepositoryHandle, collection: Collection, cursor: Option<&'a str>, limit: u16, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ObjectPage> { self.inner.list_objects(repository, collection, cursor, limit, cancel) }
    fn delete_object<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ()> { self.inner.delete_object(repository, locator, cancel) }
    fn reconcile_upload<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, resume: Option<&'a ResumeState>, cancel: &'a Cancellation)
        -> ProviderFuture<'a, UploadResolution> { self.inner.reconcile_upload(repository, intent, resume, cancel) }
    fn lookup_metadata<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, known: Option<&'a RemoteLocator>, cancel: &'a Cancellation)
        -> ProviderFuture<'a, Option<ObjectReceipt>> { self.inner.lookup_metadata(repository, intent, known, cancel) }
    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> { self.inner.head_locator(repository) }
}

pub(crate) fn small_asset(store: &mut PersistentStore, key: &str, body: &[u8]) -> String {
    let hash = risunest_sync_wire::hash(body);
    store.lww_put_managed_object(&hash, body).unwrap();
    let alias = crate::persistent_store::AssetAlias { key: key.into(), object_hash: Some(hash.clone()),
        kind: "asset".into(), size: body.len() as i64, mime: "application/octet-stream".into(),
        name: "synthetic".into(), ext: "bin".into(), inlay_type: None, width: None, height: None,
        metadata: serde_json::json!({}) };
    store.commit_asset_alias(&alias, store.revision().unwrap()).unwrap();
    hash
}
pub(crate) async fn hydrate_receiver(f: &CycleFixture, hash: &str) -> std::fs::File {
    let connected = super::previous_storage_tests::receiver_connection(f);
    let mut connections=super::connection_store::ConnectionStore::open(f.directory_b.path()).unwrap();
    match connections.read(&connected.stored.id) {
        Ok(stored)=>assert_eq!(serde_json::to_value(stored).unwrap(),serde_json::to_value(&connected.stored).unwrap()),
        Err(error) if error.kind==ErrorKind::NotFound=>connections.insert(&connected.stored).unwrap(),
        Err(error)=>panic!("synthetic hydration connection: {error:?}"),
    }
    drop(connections);
    let _source = super::lww_residency::install_test_source_connection(f.directory_b.path(), Arc::new(connected)).unwrap();
    let root = f.directory_b.path().to_owned();
    let hash = hash.to_owned();
    super::worker_observation::spawn_blocking(move || {
        super::lww_residency::hydrate(&root, &hash, &|| Ok(())).unwrap().unwrap()
    }).await.unwrap()
}

pub(crate) struct CycleFixture {
    pub directory_a: tempfile::TempDir,
    pub directory_b: tempfile::TempDir,
    pub a: PersistentStore,
    pub b: PersistentStore,
    pub provider: Arc<FakeProvider>,
    pub clock: Arc<fake::FakeLeaseClock>,
    pub sender: ExternalLwwEngine,
    pub receiver: ExternalLwwEngine,
}
impl CycleFixture {
    pub(crate) fn with_repository_id(repository_id: &str) -> Self {
        let mut fixture = Self::new();
        fixture.provider.state.lock().unwrap().repository_id = Some(repository_id.into());
        fixture.sender.repository.repository_id = repository_id.into();
        fixture.receiver.repository.repository_id = repository_id.into();
        fixture
    }

    pub(crate) fn new() -> Self {
        let directory_a = tempfile::tempdir().unwrap();
        let directory_b = tempfile::tempdir().unwrap();
        let a = PersistentStore::open(directory_a.path()).unwrap();
        let b = PersistentStore::open(directory_b.path()).unwrap();
        let provider = Arc::new(FakeProvider::with_object_limit(true, 128 * 1024 * 1024));
        let config = ConnectionConfig { provider: "synthetic".into(), profile: None,
            endpoint: "https://synthetic.invalid".into(), account_id: "fixture".into(),
            location: Default::default(), oauth_profile: None };
        let (_, capabilities) = provider.open_repository(&config, &SecretRef("fixture".into()),
            OpenMode::Existing, &Cancellation::default()).now_or_never().unwrap().unwrap();
        let descriptor = risunest_external_storage_format::format::Descriptor::new(
            "synthetic-lww-library".into(), Some(risunest_external_storage_format::format::Strategy::Cas)).unwrap();
        let now = super::runtime::now_ms();
        let clock = Arc::new(fake::FakeLeaseClock::new(now));
        crate::persistent_store::lww::test_clock::register(&a, clock.clone());
        crate::persistent_store::lww::test_clock::register(&b, clock.clone());
        let engine = |id: &str, root: &std::path::Path| ExternalLwwEngine {
            provider: provider.clone(),
            repository: fake::repository(),
            library: "synthetic-lww-library".into(),
            root_key: zeroize::Zeroizing::new([7; 32]),
            admission: Some(Admission::synthetic(now)),
            test_lease_clock: Some(clock.clone()),
            connection_id: id.into(),
            connection_root: root.into(),
            capabilities: capabilities.clone(),
            descriptor: descriptor.clone(),
        };
        let sender = engine("sender", directory_a.path());
        let receiver = engine("receiver", directory_b.path());
        Self {
            directory_a,
            directory_b,
            a,
            b,
            provider,
            clock,
            sender,
            receiver,
        }
    }
    /// Closes the sender's store and opens it again, as a restart does.
    pub(crate) fn restart_a(&mut self) {
        let placeholder = tempfile::tempdir().unwrap();
        drop(std::mem::replace(&mut self.a, PersistentStore::open(placeholder.path()).unwrap()));
        self.a = PersistentStore::open(self.directory_a.path()).unwrap();
    }
    pub(crate) fn now_ms(&self) -> u64 {
        super::leases::LeaseClock::reading(self.clock.as_ref()).wall_ms
    }
    pub(crate) async fn publish_a(&mut self) -> super::lww_engine::PublicationResult {
        self.sender
            .publish(&mut self.a, DecimalU64(0), &[], &Cancellation::default())
            .await
            .unwrap()
    }
    pub(crate) async fn receive_b(&mut self) -> usize {
        self.receiver
            .receive_and_apply(&mut self.b, DecimalU64(0), &[], &Cancellation::default())
            .await
            .unwrap()
    }
}
fn set(store: &mut PersistentStore, key: &[&str], value: serde_json::Value) {
    store
        .commit(&WorkingSetCommit {
            expected_revision: store.revision().unwrap(),
            unit_mutations: Some(vec![UnitMutation::Set {
                key: UnitKey::new(key).unwrap(),
                value,
            }]),
            ..Default::default()
        })
        .unwrap();
}

#[test]
fn cycle_clock_follows_reopened_stores_across_threads_without_affecting_other_cycles() {
    let mut first = CycleFixture::new();
    let second = CycleFixture::new();
    let first_now = first.now_ms();
    let second_now = second.now_ms();
    set(&mut first.a, &["root", "language"], serde_json::json!("ko"));
    assert_eq!(first.a.lww_clock_state().unwrap().issued.unwrap().physical_ms.0, first_now);
    first.clock.advance(60_000);
    std::thread::scope(|scope| {
        let root = first.directory_a.path();
        scope.spawn(move || {
            let mut reopened = PersistentStore::open(root).unwrap();
            set(&mut reopened, &["root", "language"], serde_json::json!("en"));
            assert_eq!(reopened.lww_clock_state().unwrap().issued.unwrap().physical_ms.0, first_now + 60_000);
        }).join().unwrap();
    });
    assert_eq!(second.now_ms(), second_now);
    first.restart_a();
    set(&mut first.a, &["root", "language"], serde_json::json!("ja"));
    assert_eq!(first.a.lww_clock_state().unwrap().issued.unwrap().physical_ms.0, first_now + 60_000);
}

fn conversation(store: &mut PersistentStore) {
    store
        .commit(&WorkingSetCommit {
            expected_revision: store.revision().unwrap(),
            unit_mutations: Some(vec![
                UnitMutation::Set {
                    key: UnitKey::new(&["exists", "character", "char"]).unwrap(),
                    value: serde_json::json!({"type":"character"}),
                },
                UnitMutation::Set {
                    key: UnitKey::new(&["exists", "conversation", "char", "conv"]).unwrap(),
                    value: serde_json::json!(true),
                },
            ]),
            ..Default::default()
        })
        .unwrap();
}
#[test]
fn two_actual_stores_publish_receive_and_quiet_listing() {
    run(async {
        let mut f = CycleFixture::new();
        assert_ne!(
            f.a.lww_clock_state().unwrap().writer_id,
            f.b.lww_clock_state().unwrap().writer_id
        );
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        let stamps = f.a.lww_read_outbox(DecimalU64(0), 100).unwrap().entries;
        let sent = f.publish_a().await;
        assert_eq!(sent.segments.0, 1);
        assert_eq!(f.provider.listing_count(), 0);
        assert_eq!(f.provider.read_count(), 0);
        assert_eq!(f.receive_b().await, 1);
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "ko");
        let state = f.b.lww_read_unit_state(DecimalU64(0), None, 100).unwrap();
        assert!(state
            .entries
            .iter()
            .any(|entry| entry.stamp == stamps[0].stamp));
        let reads = f.provider.read_count();
        let bytes = f.provider.transferred_body_bytes();
        assert_eq!(f.receive_b().await, 0);
        assert_eq!(f.provider.read_count(), reads);
        assert_eq!(f.provider.transferred_body_bytes(), bytes);
        assert_eq!(f.publish_a().await.segments.0, 0);
    })
}
fn assert_no_publication_pins_left(f: &CycleFixture) {
    use crate::asset_repository::job_pins::{collect_durable_cas_job_roots, durable_cas_job_ids};
    assert_eq!(durable_cas_job_ids(f.directory_a.path()).unwrap(), Vec::<String>::new());
    let roots = collect_durable_cas_job_roots(f.directory_a.path());
    assert!(roots.blockers.is_empty(), "{:?}", roots.blockers);
}
#[test]
fn a_publication_that_cannot_seal_its_asset_pins_releases_them() {
    run(async {
        let mut f = CycleFixture::new();
        small_asset(&mut f.a, "synthetic-seal-failure", &[43; 4096]);
        rusqlite::Connection::open(f.directory_a.path().join("persistent/persistent.sqlite")).unwrap()
            .execute_batch("CREATE TRIGGER fail_publication_seal BEFORE INSERT ON asset_objects
                BEGIN SELECT RAISE(ABORT, 'synthetic'); END;").unwrap();
        assert!(f.sender.publish(&mut f.a, DecimalU64(0), &[], &Cancellation::default()).await.is_err());
        assert_no_publication_pins_left(&f);
    })
}
#[test]
fn a_publication_that_cannot_save_its_segment_releases_its_asset_pins() {
    run(async {
        let mut f = CycleFixture::new();
        small_asset(&mut f.a, "synthetic-persist-failure", &[44; 4096]);
        rusqlite::Connection::open(f.directory_a.path().join("persistent/device.sqlite")).unwrap()
            .execute_batch("CREATE TRIGGER fail_publication_persist BEFORE INSERT ON external_lww_segments
                BEGIN SELECT RAISE(ABORT, 'synthetic'); END;").unwrap();
        assert!(f.sender.publish(&mut f.a, DecimalU64(0), &[], &Cancellation::default()).await.is_err());
        assert_no_publication_pins_left(&f);
    })
}
#[test]
fn a_saved_publication_keeps_its_asset_pins_across_a_page_reload_until_it_is_sent() {
    use crate::asset_repository::commands::{CasJobOwnerProbe, DurableCasJobState};
    use crate::asset_repository::job_pins::durable_cas_job_ids;
    run(async {
        let mut f = CycleFixture::new();
        small_asset(&mut f.a, "synthetic-saved-publication", &[45; 4096]);
        f.provider.fail_upload_number(f.provider.upload_count() + 1, ErrorKind::Transient);
        assert!(f.sender.publish(&mut f.a, DecimalU64(0), &[], &Cancellation::default()).await.is_err());
        let root = f.directory_a.path().to_owned();
        let journals = durable_cas_job_ids(&root).unwrap();
        assert_eq!(journals.len(), 1);
        let saved: bool = rusqlite::Connection::open(root.join("persistent/device.sqlite")).unwrap()
            .query_row("SELECT EXISTS(SELECT 1 FROM external_lww_segments WHERE json_extract(metadata,'$.assetJob.jobId')=?1)",
                [&journals[0]], |row| row.get(0)).unwrap();
        assert!(saved);
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        let (publication, _) = f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap();
        f.a.device_store().unwrap().connection().execute(
            "UPDATE external_lww_segments SET metadata=?1 WHERE target=?2 AND writer=?3 AND seq=?4",
            rusqlite::params![serde_json::to_string(&publication).unwrap(), publication.target, writer, publication.seq.0.to_string()],
        ).unwrap();
        f.restart_a();
        let native_jobs = || -> std::result::Result<Vec<crate::native_file_jobs::JobStatus>, String> { Ok(Vec::new()) };
        let open_store = || f.a.open_native_job_store().map_err(|error| error.to_string());
        DurableCasJobState::default().sweep_after_page_start(&root, &CasJobOwnerProbe {
            native_jobs: &native_jobs,
                device_job_owned: &|_| Ok(false),
                external_job_active: &|_| Ok(false),
            open_store: &open_store,
        }).unwrap();
        assert_eq!(durable_cas_job_ids(&root).unwrap(), journals);
        assert_eq!(f.publish_a().await.segments.0, 1);
        assert_no_publication_pins_left(&f);
    })
}
#[test]
fn publication_uses_inline_and_local_sources_without_opening_an_unused_server_registry() {
    run(async {
        let mut f=CycleFixture::new();
        set(&mut f.a,&["root","language"],serde_json::json!("synthetic-language"));
        let body=b"synthetic local publication body";
        let hash=small_asset(&mut f.a,"synthetic-local-registry",body);
        let registry=crate::server_sync::residency::Residency::path(f.directory_a.path());
        std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
        std::fs::write(&registry,b"synthetic invalid unused SQLite registry").unwrap();
        assert!(crate::server_sync::residency::Residency::open(f.directory_a.path()).is_err());
        assert_eq!(f.publish_a().await.segments.0,1);
        assert_eq!(f.receive_b().await,1);
        let mut restored=hydrate_receiver(&f,&hash).await;
        let mut bytes=Vec::new();
        std::io::Read::read_to_end(&mut restored,&mut bytes).unwrap();
        assert_eq!(bytes,body);
    })
}

#[test]
fn small_assets_use_authenticated_catalogs_and_present_bootstrap_reads_no_bodies() {
    for repository_id in repository_id_cases() { run(async {
        use crate::asset_repository::body_io::{reset_body_io, take_body_io, register_object_purpose, BodyPurpose};
        let mut f = CycleFixture::with_repository_id(&repository_id);
        let body = vec![41; 128 * 1024];
        let hash = small_asset(&mut f.a, "synthetic-packed", &body);
        let second = vec![42; 64 * 1024];
        let second_hash = small_asset(&mut f.a, "synthetic-packed-second", &second);
        let transfer = HeldAssetTransfer::new(f.provider.clone());
        transfer.held.store(false, std::sync::atomic::Ordering::SeqCst);
        f.sender.provider = transfer.clone();
        assert_eq!(f.publish_a().await.segments.0, 1);
        let cancel = Cancellation::default();
        let receipts = f.sender.listing(&cancel).await.unwrap();
        let bytes = super::lww_engine::read_bytes(f.provider.as_ref(), &f.sender.repository, &receipts[0].locator, &cancel).await.unwrap();
        let (writer, seq, _) = parse_segment_object_id(&receipts[0].locator.object).unwrap();
        let payload = lww_segment::open(&bytes, &f.sender.library, writer, seq, &f.sender.root_key).unwrap();
        assert_eq!(payload.asset_catalogs.len(), 1);
        let stored_id = &payload.asset_catalogs[0].header.repository_id;
        assert!(stored_id.len() <= 128);
        assert_eq!(stored_id == &repository_id, repository_id.len() <= 128);
        assert!(payload.large_bodies.is_empty());
        assert!(payload.message_pages.is_empty());
        let encoded = payload.encode().unwrap();
        let text = std::str::from_utf8(&encoded).unwrap();
        assert!(text.contains("\"assetCatalogs\""));
        assert!(!text.contains("smallBodies"));
        assert!(!text.contains(&base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &body)));
        let mut shape: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        shape.as_object_mut().unwrap().remove("dataCatalogs");
        assert!(lww_segment::Segment::decode(&risunest_sync_wire::canonical::encode(&shape).unwrap()).is_err());
        shape["dataCatalogs"]=serde_json::json!([]);
        shape.as_object_mut().unwrap().remove("assetCatalogs");
        assert!(lww_segment::Segment::decode(&risunest_sync_wire::canonical::encode(&shape).unwrap()).is_err());
        shape["assetCatalogs"] = serde_json::json!([]); shape["smallBodies"] = serde_json::json!({});
        assert!(lww_segment::Segment::decode(&risunest_sync_wire::canonical::encode(&shape).unwrap()).is_err());
        let packs = transfer.attempts.lock().unwrap().iter().map(|intent| intent.object_id.clone()).collect::<Vec<_>>();
        assert_eq!(packs.len(), 1, "small Assets share the existing pack container");
        f.b.lww_put_managed_object(&hash, &body).unwrap();
        f.b.lww_put_managed_object(&second_hash, &second).unwrap();
        let pack_reads = packs.iter().map(|id| f.provider.read_attempts(id)).collect::<Vec<_>>();
        reset_body_io();
        for hash in [&hash, &second_hash] { register_object_purpose(hash, BodyPurpose::Asset); }
        let directory = tempfile::tempdir().unwrap();
        let mut state = f.receiver.published_state(&mut f.b, directory.path(), &cancel).await.unwrap();
        f.receiver.stage_published_objects(&mut f.b, &mut state, directory.path(), &cancel).await.unwrap();
        let work = take_body_io();
        assert!(work.complete(), "{work:?}");
        assert_eq!(work.asset_work(), Default::default());
        assert_eq!(packs.iter().map(|id| f.provider.read_attempts(id)).collect::<Vec<_>>(), pack_reads);
        assert!(super::lww_residency::stat(f.directory_b.path(), &hash).unwrap().is_some());
        assert!(super::lww_residency::stat(f.directory_b.path(), &second_hash).unwrap().is_some());
        assert_eq!(f.receive_b().await, 1);
    }); }
}

#[test]
fn packed_asset_uncertainty_reopens_frozen_job_and_keeps_original_pack_bytes() {
    for repository_id in repository_id_cases() { run(async {
        let mut f = CycleFixture::with_repository_id(&repository_id);
        let body = vec![43; 96 * 1024];
        let hash = small_asset(&mut f.a, "synthetic-frozen-packed", &body);
        let held = HeldAssetTransfer::new(f.provider.clone());
        held.lose_pack_response.store(true, std::sync::atomic::Ordering::SeqCst);
        held.cancel_after_pack_loss.store(true, std::sync::atomic::Ordering::SeqCst);
        f.sender.provider = held.clone();
        let reached = std::cell::Cell::new(false);
        let release = async { held.entered.notified().await; reached.set(true); held.resume.notify_one(); Ok::<_, ProviderError>(()) };
        let cancel = Cancellation::default();
        let publication = async {
            let result = f.sender.publish(&mut f.a, 0.into(), &[], &cancel).await;
            if !reached.get() { return Err(result.err().unwrap_or_else(|| ProviderError::new(ErrorKind::Corrupt))); }
            Ok(result)
        };
        let (result, ()) = tokio::try_join!(publication, release).unwrap();
        assert!(reached.get());
        assert!(result.is_err());
        assert!(cancel.check().is_err());
        let cancel = Cancellation::default();
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        let (pending, segment_bytes) = f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap();
        assert!(!pending.sealed); assert!(segment_bytes.is_empty());
        assert_eq!(pending.assets, vec![crate::persistent_store::external_lww::FrozenAsset { content_hash: hash.clone(), byte_length: body.len() as u64, local_pin: true, remote_source: None, server_source: None }]);
        let original_job = pending.asset_job.clone().unwrap();
        let original_intent = held.attempts.lock().unwrap()[0].clone();
        let original_ciphertext = f.provider.contents(&original_intent.object_id).unwrap();
        assert_eq!(f.provider.uploaded_ids().iter().filter(|id| parse_segment_object_id(id).is_ok()).count(), 0);
        f.a = PersistentStore::open(f.directory_a.path()).unwrap();
        let reopened = f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap().0;
        assert_eq!(reopened.assets, pending.assets); assert!(reopened.asset_job == Some(original_job));
        assert_eq!(f.publish_a().await.segments.0, 1);
        assert_eq!(f.provider.contents(&original_intent.object_id).unwrap(), original_ciphertext);
        assert!(held.attempts.lock().unwrap().iter().all(|intent| intent.object_id == original_intent.object_id && intent.sha256 == original_intent.sha256));
        assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), 2);
        assert!(f.a.lww_read_outbox(0.into(), 100).unwrap().entries.is_empty());
        let directory = tempfile::tempdir().unwrap();
        let mut state = f.receiver.published_state(&mut f.b, directory.path(), &cancel).await.unwrap();
        f.receiver.stage_published_objects(&mut f.b, &mut state, directory.path(), &cancel).await.unwrap();
        assert!(!f.b.external_lww_object_is_local(&hash).unwrap());
        assert_eq!(super::lww_residency::stat(f.directory_b.path(), &hash).unwrap(), Some(body.len() as u64));
    }); }
}

#[test]
fn server_held_publication_keeps_captured_source_and_rejects_changed_authority() {
    use crate::server_sync::{lww_tests::LocalServerFixture, residency::{AssetPolicy, Residency}};
    for large in [false, true] {
        for stale in [false, true] {
            let mut f = CycleFixture::new();
            let server = LocalServerFixture::new();
            let core = server.client(&f.a);
            let body = vec![67; if large { 5 * 1024 * 1024 } else { 96 * 1024 }];
            let hash = small_asset(&mut f.a, "synthetic-server-frozen", &body);
            crate::server_sync::lww_tests::drain_publications(&core, &mut f.a, &[]).unwrap();
            f.a.asset_residency_set_policy(AssetPolicy::Remote, || Ok(())).unwrap();
            f.a.asset_residency_evict(|| Ok(())).unwrap();
            assert!(!f.a.external_lww_object_is_local(&hash).unwrap());
            let original = Residency::open(f.directory_a.path()).unwrap().object(&hash, None).unwrap().unwrap();
            let alias = crate::persistent_store::AssetAlias { key: "synthetic-server-frozen".into(),
                object_hash: Some(hash.clone()), kind: "asset".into(), size: body.len() as i64,
                mime: "application/octet-stream".into(), name: "Synthetic changed alias".into(), ext: "bin".into(),
                inlay_type: None, width: None, height: None, metadata: serde_json::json!({}) };
            f.a.commit_asset_alias(&alias, f.a.revision().unwrap()).unwrap();
            let original_pending = f.a.lww_read_outbox(0.into(), 100).unwrap().entries;
            assert_eq!(original_pending.len(), 1);
            let held = HeldAssetTransfer::new(f.provider.clone());
            held.held.store(false, std::sync::atomic::Ordering::SeqCst);
            let root = f.directory_a.path().to_owned();
            let target_scope = f.sender.target_scope();
            let writer = f.a.lww_clock_state().unwrap().writer_id;
            let source_hash = hash.clone();
            let original_metadata = serde_json::to_value(&original).unwrap();
            let mut changed_config = original.config.clone();
            changed_config.endpoint = format!("{}/changed-source", server.endpoint);
            let reached = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let reached_callback = reached.clone();
            *held.before_lease.lock().unwrap() = Some(Box::new(move || {
                let mut store = PersistentStore::open(&root).unwrap();
                let (capture, bytes) = store.external_lww_pending(&target_scope, &writer).unwrap().unwrap();
                assert!(!capture.sealed);
                let asset = capture.assets.iter().find(|asset| asset.content_hash == source_hash).unwrap();
                assert!(!asset.local_pin && asset.remote_source.is_none());
                assert_eq!(serde_json::to_value(asset.server_source.as_ref().unwrap()).unwrap(), original_metadata);
                let mut altered_capture = capture.clone();
                altered_capture.assets[0].server_source.as_mut().unwrap().context.push('x');
                assert!(store.external_lww_persist(&altered_capture, &bytes).is_err());
                Residency::open(&root).unwrap().replace_access_config(&changed_config).unwrap();
                assert_ne!(serde_json::to_value(Residency::open(&root).unwrap().object(&source_hash, None).unwrap().unwrap()).unwrap(), original_metadata);
                if stale {
                    let state = store.lww_binding_state().unwrap();
                    let target = crate::persistent_store::sync_selection::SyncTarget::External("changed-source-target".into());
                    let inspection = store.register_lww_binding_inspection(state.target_authority, &target, "synthetic-next-repository", "synthetic-next-library").unwrap();
                    store.switch_lww_binding(&crate::persistent_store::sync_selection::SwitchBindingRequest {
                        initial_publication: false,
                        header: crate::persistent_store::lww::Header { binding_authority: state.target_authority, request_id: "frozen-source-authority".into() },
                        expected_selection_epoch: state.selection_epoch, target, inspection_id: Some(inspection),
                    }).unwrap();
                }
                reached_callback.store(true, std::sync::atomic::Ordering::SeqCst);
            }));
            f.sender.provider = held.clone();
            crate::server_sync::hash_metrics::reset_hash_metrics();
            super::worker_observation::begin();
            let result = run(f.sender.publish(&mut f.a, 0.into(), &[], &Cancellation::default()));
            let workers = super::worker_observation::take();
            let server_hashes = crate::server_sync::hash_metrics::take_hash_metrics();
            assert!(reached.load(std::sync::atomic::Ordering::SeqCst));
            let segments = f.provider.uploaded_ids().iter().filter(|id| parse_segment_object_id(id).is_ok()).count();
            if stale {
                assert_eq!(result.err().expect("changed authority must reject publication").kind, ErrorKind::PreconditionFailed);
                assert_eq!(segments, 0); assert!(held.attempts.lock().unwrap().is_empty());
                let writer = f.a.lww_clock_state().unwrap().writer_id;
                let pending = f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap().0;
                assert_eq!(pending.entries, original_pending);
                assert!(!pending.sealed && !pending.dispatched && pending.bodies.iter().all(|body| body.sha256.is_empty()));
                assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), 1);
            } else {
                assert!(!workers.is_empty());
                assert!(workers.iter().all(|worker| worker.completed && worker.hashes.incomplete.is_empty()));
                assert!(!server_hashes.incomplete);
                assert!(server_hashes.domains["c_cache_stream_verify"].bytes >= body.len() as u64);
                assert_eq!(result.unwrap().segments.0, 1); assert_eq!(segments, 1);
                assert!(f.a.lww_read_outbox(0.into(), 100).unwrap().entries.is_empty());
                assert!(!f.a.external_lww_object_is_local(&hash).unwrap());
                run(f.receiver.receive_and_apply(&mut f.b, 0.into(), &[], &Cancellation::default())).unwrap();
                assert_eq!(super::lww_residency::stat(f.directory_b.path(), &hash).unwrap(), Some(body.len() as u64));
            }
        }
    }
}

#[test]
fn real_publish_and_receive_continue_while_checkpoint_inputs_are_paused() {
    run(async {
        let mut f=CycleFixture::new();
        set(&mut f.a,&["root","language"],serde_json::json!("ko")); f.publish_a().await;
        let job=tempfile::tempdir().unwrap(); let id="00000000-0000-4000-8000-000000000097";
        let barrier=super::lww_compaction::CompactionBarrier::new();
        super::lww_compaction::install_compaction_barrier(id,barrier.clone());
        let writer=f.a.lww_clock_state().unwrap().writer_id;
        let caps=fake::capabilities(true); let cancel=Cancellation::default();
        let compactor=ExternalLwwEngine{provider:f.provider.clone(),repository:fake::repository(),library:f.sender.library.clone(),root_key:zeroize::Zeroizing::new([7;32]),admission:f.sender.admission,test_lease_clock:f.sender.test_lease_clock.clone(),connection_id:"compactor".into(),connection_root:job.path().into(),capabilities:f.sender.capabilities.clone(),descriptor:f.sender.descriptor.clone()};
        let compactor_directory=tempfile::tempdir().unwrap();let mut compactor_store=PersistentStore::open(compactor_directory.path()).unwrap();
        let compaction=compactor.compact_published(&mut compactor_store,job.path(),id,&writer,&caps,&cancel,None);
        let routine=async {
            barrier.reached.notified().await;
            set(&mut f.a,&["root","language"],serde_json::json!("en"));
            assert_eq!(f.sender.publish(&mut f.a,DecimalU64(0),&[],&cancel).await.unwrap().segments.0,1);
            assert_eq!(f.receiver.receive_and_apply(&mut f.b,DecimalU64(0),&[],&cancel).await.unwrap(),2);
            barrier.resume.notify_one();
        };
        let (completed,())=tokio::join!(compaction,routine);
        let completed=completed.unwrap(); let (_,snapshot)=f.sender.checkpoint(&completed.reference.receipt,&cancel).await.unwrap();
        assert_eq!(snapshot.covered_prefixes.get(&writer).unwrap().0,1);
        assert_eq!(f.b.read_root(None).unwrap().value["language"],serde_json::json!("en"));
    });
}
#[test]
fn reused_data_roots_keep_controls_reachable_and_skip_present_catalog_bodies() {
    run(async {
        let mut f=CycleFixture::new(); conversation(&mut f.a);
        let transfer=HeldAssetTransfer::new(f.provider.clone());
        transfer.held.store(false,std::sync::atomic::Ordering::SeqCst);
        f.sender.provider=transfer.clone();
        let messages=(0..40).map(|index|serde_json::json!({"chatId":format!("synthetic-root-{index}"),"data":"x".repeat(128*1024)})).collect();
        f.a.commit(&WorkingSetCommit{expected_revision:f.a.revision().unwrap(),
            conversations:Some(vec![ConversationMutation::ReplaceRange{character_id:"char".into(),conversation_id:"conv".into(),
                start:0,delete_count:0,messages,conversation:None,configured_index:None}]),..Default::default()}).unwrap();
        f.publish_a().await; assert_eq!(f.receive_b().await,1);
        let first=f.sender.listing(&Cancellation::default()).await.unwrap().remove(0);
        let (writer,seq,_)=parse_segment_object_id(&first.locator.object).unwrap();
        let first_payload=lww_segment::open(&f.provider.contents(&first.locator.object).unwrap(),&f.sender.library,writer,seq,&f.sender.root_key).unwrap();
        assert_eq!(first_payload.data_catalogs.len(),1);
        let root=first_payload.data_catalogs[0].clone();
        let hashes=f.a.external_lww_verified_data_catalog(&f.sender.target_scope(),&root).unwrap().unwrap();
        assert!(hashes.len()>20);
        assert_eq!(f.a.external_lww_verified_control_size(&hashes[0]).unwrap(),
            Some(f.a.lww_object_body(&hashes[0]).unwrap().unwrap().len() as u64));
        let before=std::iter::once(root.locator.object.clone()).chain(transfer.attempts.lock().unwrap().iter().map(|intent|intent.object_id.clone()))
            .map(|id|(id.clone(),f.provider.read_attempts(&id))).collect::<Vec<_>>();
        let old_pack_attempts=transfer.attempts.lock().unwrap().len();
        f.a.commit(&WorkingSetCommit{expected_revision:f.a.revision().unwrap(),
            conversations:Some(vec![ConversationMutation::ReplaceRange{character_id:"char".into(),conversation_id:"conv".into(),
                start:40,delete_count:0,messages:vec![serde_json::json!({"chatId":"synthetic-root-append","data":"append"})],
                conversation:None,configured_index:None}]),..Default::default()}).unwrap();
        assert_eq!(f.publish_a().await.segments.0,1);
        assert_eq!(transfer.attempts.lock().unwrap().len(),old_pack_attempts);
        let next=f.sender.listing(&Cancellation::default()).await.unwrap().into_iter()
            .find(|receipt|parse_segment_object_id(&receipt.locator.object).unwrap().1==2).unwrap();
        let next_payload=lww_segment::open(&f.provider.contents(&next.locator.object).unwrap(),&f.sender.library,writer,2,&f.sender.root_key).unwrap();
        assert!(next_payload.data_catalogs.contains(&root));
        assert_eq!(f.receive_b().await,1);
        for (id,reads) in &before {assert_eq!(f.provider.read_attempts(id),*reads,"known root and its present controls must not be downloaded again");}
        assert_eq!(f.b.read_conversation("char","conv",None).unwrap().unwrap().value["message"].as_array().unwrap().len(),41);
        // The receive replaced pages the root lists, so the receiver now holds them marked.
        f.a.commit(&WorkingSetCommit{expected_revision:f.a.revision().unwrap(),
            conversations:Some(vec![ConversationMutation::ReplaceRange{character_id:"char".into(),conversation_id:"conv".into(),
                start:41,delete_count:0,messages:vec![serde_json::json!({"chatId":"synthetic-root-append-again","data":"append"})],
                conversation:None,configured_index:None}]),..Default::default()}).unwrap();
        assert_eq!(f.publish_a().await.segments.0,1);
        for (id,reads) in &before {assert_eq!(f.provider.read_attempts(id),*reads,"a publisher downloaded the pages its commit replaced again");}
        let third=f.sender.listing(&Cancellation::default()).await.unwrap().into_iter()
            .find(|receipt|parse_segment_object_id(&receipt.locator.object).unwrap().1==3).unwrap();
        assert!(lww_segment::open(&f.provider.contents(&third.locator.object).unwrap(),&f.sender.library,writer,3,&f.sender.root_key).unwrap().data_catalogs.contains(&root));
        assert_eq!(f.receive_b().await,1);
        for (id,reads) in &before {assert_eq!(f.provider.read_attempts(id),*reads,"a receiver downloaded the pages its receive replaced again");}
        assert_eq!(f.b.read_conversation("char","conv",None).unwrap().unwrap().value["message"].as_array().unwrap().len(),42);
        let mut changed=root.clone(); changed.plaintext_sha256=[0xaa;32];
        assert!(f.a.external_lww_verified_data_catalog(&f.sender.target_scope(),&changed).is_err());
        let mut changed_hashes=hashes.clone(); changed_hashes.push("a".repeat(64));
        assert!(f.a.external_lww_witness_data_catalog(&f.sender.target_scope(),&root,&changed_hashes).is_err());
    });
}
#[test]
fn oversized_control_pages_use_frozen_data_packs_and_preserve_newer_edits_on_retry() {
    run(async {
        let mut f=CycleFixture::new();
        conversation(&mut f.a);
        let messages=(0..224).map(|index|serde_json::json!({
            "chatId":format!("synthetic-large-{index}"),"data":"x".repeat(225*1024),
        })).collect::<Vec<_>>();
        f.a.commit(&WorkingSetCommit { expected_revision:f.a.revision().unwrap(),
            conversations:Some(vec![ConversationMutation::ReplaceRange { character_id:"char".into(),conversation_id:"conv".into(),
                start:0,delete_count:0,messages,conversation:None,configured_index:None }]),..Default::default() }).unwrap();
        let key=UnitKey::new(&["messages","char","conv"]).unwrap();
        let original=f.a.lww_read_outbox(0.into(),4096).unwrap().entries.into_iter().find(|entry|entry.key==key).unwrap();
        let risunest_sync_wire::unit::UnitValue::Object { descriptor,.. }=&original.value else { panic!("messages must use a manifest") };
        let manifest=risunest_external_storage_format::message_pages::MessageManifest::decode(
            &f.a.lww_object_body(&descriptor.object_hash).unwrap().unwrap()).unwrap();
        assert!(manifest.pages.iter().all(|page|page.byte_length.0<=256*1024));
        assert!(manifest.pages.iter().map(|page|page.byte_length.0).sum::<u64>()>48*1024*1024);
        let transfer=HeldAssetTransfer::new(f.provider.clone());
        transfer.held.store(false,std::sync::atomic::Ordering::SeqCst);
        transfer.lose_pack_response.store(true,std::sync::atomic::Ordering::SeqCst);
        transfer.cancel_after_pack_loss.store(true,std::sync::atomic::Ordering::SeqCst);
        f.sender.provider=transfer.clone();
        assert!(f.sender.publish(&mut f.a,0.into(),&[],&Cancellation::default()).await.is_err());
        let writer=f.a.lww_clock_state().unwrap().writer_id;
        let (pending,sealed)=f.a.external_lww_pending(&f.sender.target_scope(),&writer).unwrap().unwrap();
        assert!(!pending.sealed && sealed.is_empty());
        assert!(!pending.controls.is_empty());
        assert_eq!(pending.seq.0,1);
        assert!(pending.entries.iter().any(|entry|entry==&original));
        let pack=transfer.attempts.lock().unwrap()[0].object_id.clone();
        let original_pack=f.provider.contents(&pack).unwrap();
        let mut changed=pending.clone(); changed.controls[0].byte_length+=1;
        assert!(f.a.external_lww_persist(&changed,&sealed).is_err());
        f.a.commit(&WorkingSetCommit { expected_revision:f.a.revision().unwrap(),
            conversations:Some(vec![ConversationMutation::ReplaceRange { character_id:"char".into(),conversation_id:"conv".into(),
                start:224,delete_count:0,messages:vec![serde_json::json!({"chatId":"synthetic-newer","data":"newer"})],
                conversation:None,configured_index:None }]),..Default::default() }).unwrap();
        f.a=PersistentStore::open(f.directory_a.path()).unwrap();
        let reopened=f.a.external_lww_pending(&f.sender.target_scope(),&writer).unwrap().unwrap().0;
        assert_eq!(reopened.controls,pending.controls);
        assert!(reopened.asset_job==pending.asset_job);
        assert_eq!(f.publish_a().await.segments.0,2,"exact older ACK must leave the newer messages version pending");
        assert_eq!(f.provider.contents(&pack).unwrap(),original_pack);
        let mut segments=f.sender.listing(&Cancellation::default()).await.unwrap();
        segments.sort_by_key(|receipt|parse_segment_object_id(&receipt.locator.object).unwrap().1);
        assert_eq!(segments.len(),2);
        let first=&segments[0];
        let bytes=f.provider.contents(&first.locator.object).unwrap();
        assert!(bytes.len()<=lww_segment::SEGMENT_BYTES);
        let payload=lww_segment::open(&bytes,&f.sender.library,&writer,1,&f.sender.root_key).unwrap();
        assert_eq!(payload.data_catalogs.len(),1);
        let published=payload.changes.iter().find(|change|change.key==key).unwrap();
        assert_eq!(published.stamp,original.stamp);
        assert_eq!(published.value,original.value);
        let catalog=&payload.data_catalogs[0];
        let original_catalog=f.provider.contents(&catalog.locator.object).unwrap();
        let mut corrupt=original_catalog.clone(); *corrupt.last_mut().unwrap()^=1;
        f.provider.seed(&catalog.locator.object,ObjectRole::Catalog,corrupt);
        let revision=f.b.revision().unwrap();
        assert_eq!(f.receiver.receive_requests(&mut f.b,0.into(),&Cancellation::default()).await.err().unwrap().kind,ErrorKind::Corrupt);
        assert_eq!(f.b.revision().unwrap(),revision);
        assert!(f.b.lww_receive_progress(0.into()).unwrap().is_empty());
        f.provider.seed(&catalog.locator.object,ObjectRole::Catalog,original_catalog);
        assert_eq!(f.receive_b().await,2);
        let conversation=f.b.read_conversation("char","conv",None).unwrap().unwrap().value;
        assert_eq!(conversation["message"].as_array().unwrap().len(),225);
        assert_eq!(conversation["message"][224]["chatId"],"synthetic-newer");
        assert!(f.a.lww_read_outbox(0.into(),4096).unwrap().entries.is_empty());
    });
}
#[test]
fn indivisible_message_above_segment_cap_uses_chunked_data_control_carriage() {
    run(async {
        let mut f=CycleFixture::new(); conversation(&mut f.a);
        f.a.commit(&WorkingSetCommit { expected_revision:f.a.revision().unwrap(),
            conversations:Some(vec![ConversationMutation::ReplaceRange { character_id:"char".into(),conversation_id:"conv".into(),
                start:0,delete_count:0,messages:vec![serde_json::json!({"chatId":"synthetic-indivisible","data":"x".repeat(65*1024*1024)})],
                conversation:None,configured_index:None }]),..Default::default() }).unwrap();
        assert_eq!(f.publish_a().await.segments.0,1);
        let object=f.sender.listing(&Cancellation::default()).await.unwrap().remove(0);
        let bytes=f.provider.contents(&object.locator.object).unwrap();
        assert!(bytes.len()<=lww_segment::SEGMENT_BYTES);
        let (writer,seq,_)=parse_segment_object_id(&object.locator.object).unwrap();
        let payload=lww_segment::open(&bytes,&f.sender.library,writer,seq,&f.sender.root_key).unwrap();
        assert_eq!(payload.data_catalogs.len(),1);
        assert_eq!(f.receive_b().await,1);
        let conversation=f.b.read_conversation("char","conv",None).unwrap().unwrap().value;
        assert_eq!(conversation["message"][0]["data"].as_str().unwrap().len(),65*1024*1024);
    });
}
#[test]
fn segment_limit_counts_encryption_framing_at_the_exact_boundary() {
    let mut low=0;
    let mut high=lww_segment::SEGMENT_BYTES;
    while low<high {
        let middle=low+(high-low).div_ceil(2);
        if lww_segment::sealed_length(middle).is_ok() { low=middle; } else { high=middle-1; }
    }
    assert!(low<lww_segment::SEGMENT_BYTES);
    assert!(lww_segment::sealed_length(low).unwrap()<=lww_segment::SEGMENT_BYTES as u64);
    assert_eq!(lww_segment::sealed_length(low+1).unwrap_err().kind,ErrorKind::FileTooLarge);
    assert_eq!(lww_segment::sealed_length(lww_segment::SEGMENT_BYTES).unwrap_err().kind,ErrorKind::FileTooLarge);
    let segment=lww_segment::Segment::new("synthetic-library","00000000-0000-4000-8000-000000000001",1);
    let plain=segment.encode().unwrap();
    let (sealed,_)=lww_segment::seal(&segment,&[7;32]).unwrap();
    assert_eq!(sealed.len() as u64,lww_segment::sealed_length(plain.len()).unwrap());
}
#[test]
fn restart_uses_retained_segments_and_bootstraps_only_after_required_retirement() {
    run(async {
        for retire_required in [false,true] {
            let mut f=CycleFixture::new();
            set(&mut f.a,&["root","language"],serde_json::json!("one"));
            f.publish_a().await;
            assert_eq!(f.receive_b().await,1);
            set(&mut f.a,&["root","language"],serde_json::json!("two"));
            f.publish_a().await;
            let required=f.sender.listing(&Cancellation::default()).await.unwrap().into_iter()
                .find(|receipt| parse_segment_object_id(&receipt.locator.object).unwrap().1==2).unwrap();
            let required_bytes=f.provider.contents(&required.locator.object).unwrap();
            let job=tempfile::tempdir().unwrap();
            let writer=f.a.lww_clock_state().unwrap().writer_id;
            let completed=f.sender.compact_published(&mut f.a,job.path(),"00000000-0000-4000-8000-000000000090",
                &writer,&f.sender.capabilities,&Cancellation::default(),None).await.unwrap();
            let dependencies=completed.referenced_objects.iter()
                .filter(|object|matches!(object.role,ObjectRole::Pack|ObjectRole::Catalog))
                .map(|object|object.receipt.locator.object.clone()).collect::<Vec<_>>();
            assert!(!dependencies.is_empty());
            let before=dependencies.iter().map(|id|f.provider.read_attempts(id)).collect::<Vec<_>>();
            set(&mut f.b,&["root","loreBookDepth"],serde_json::json!(7));
            f.b=PersistentStore::open(f.directory_b.path()).unwrap();
            if retire_required { f.provider.forget(&required.locator.object); }
            assert_eq!(f.receive_b().await,1);
            assert_eq!(f.b.read_root(None).unwrap().value["language"],"two");
            assert_eq!(f.b.read_root(None).unwrap().value["loreBookDepth"],7);
            assert!(!f.b.lww_read_outbox(0.into(),100).unwrap().entries.is_empty());
            let after=dependencies.iter().map(|id|f.provider.read_attempts(id)).collect::<Vec<_>>();
            if retire_required {
                assert!(after.iter().zip(&before).any(|(after,before)|after>before));
            } else {
                assert_eq!(after,before,"retained contiguous history must not read checkpoint catalogs/data packs");
                let (writer,seq,_)=parse_segment_object_id(&required.locator.object).unwrap();
                let mut variant=lww_segment::open(&required_bytes,&f.sender.library,writer,seq,&f.sender.root_key).unwrap();
                variant.changes.clear();
                let (bytes,_)=lww_segment::seal(&variant,&f.sender.root_key).unwrap();
                let id=segment_object_id(writer,seq,&lww_segment::digest(&bytes)).unwrap();
                f.provider.seed(&id,ObjectRole::Segment,bytes);
                let progress=f.b.lww_receive_progress(0.into()).unwrap();
                assert_eq!(f.receiver.receive_requests(&mut f.b,0.into(),&Cancellation::default()).await.err().unwrap().kind,ErrorKind::Corrupt);
                assert_eq!(serde_json::to_value(f.b.lww_receive_progress(0.into()).unwrap()).unwrap(),serde_json::to_value(progress).unwrap());
            }
        }
    });
}
#[test]
fn checkpoint_uses_only_published_units_and_restores_after_segment_retirement() {
    for repository_id in repository_id_cases() { run(async {
        let mut f=CycleFixture::with_repository_id(&repository_id);
        set(&mut f.a,&["root","language"],serde_json::json!("ko"));
        f.publish_a().await;
        set(&mut f.a,&["root","language"],serde_json::json!("en"));
        let job=tempfile::tempdir().unwrap();
        let writer=f.a.lww_clock_state().unwrap().writer_id;
        let completed=f.sender.compact_published(&mut f.a,job.path(),"00000000-0000-4000-8000-000000000099",&writer,&fake::capabilities(true),&Cancellation::default(),None).await.unwrap();
        assert_eq!(completed.reference.role,ObjectRole::Snapshot);
        let snapshots=f.sender.snapshot_listing(&Cancellation::default()).await.unwrap();
        assert_eq!(snapshots.len(),1);
        let (_,snapshot)=f.sender.checkpoint(&snapshots[0],&Cancellation::default()).await.unwrap();
        assert_eq!(snapshot.covered_prefixes.values().next().unwrap().0,1);
        let backup_id="00000000-0000-4000-8000-000000000096";
        // This input exercises metadata classification; its copied catalogs are not restored.
        let mut backup_library=snapshot.library.clone();
        for object in [&mut backup_library.record_catalog,&mut backup_library.asset_catalog] {
            object.header.repository_id=f.sender.descriptor.repository_id.clone();
            object.ciphertext_length=risunest_external_storage_format::snapshot::envelope_length(&object.header).unwrap();
        }
        let original_units=backup_library.record_catalog.clone();
        let backup=risunest_external_storage_format::control::BackupBundleDocument::new(
            f.sender.descriptor.repository_id.clone(),backup_id.into(),
            risunest_external_storage_format::control::BundleSource::Device{writer_id:f.a.lww_clock_state().unwrap().writer_id},
            super::runtime::now_ms(),Some(risunest_sync_wire::head::Sequence::from(0u64)),None,None,
            backup_library,std::collections::BTreeMap::new(),
            Some(original_units),
        ).unwrap();
        let backup_plain=backup.encode(risunest_external_storage_format::snapshot::MAX_METADATA_BYTES).unwrap();
        let backup_object=format!("snapshot-{backup_id}");
        let header=risunest_external_storage_format::snapshot::PublicObjectHeader::new(f.sender.descriptor.repository_id.clone(),backup_object.clone(),risunest_external_storage_format::snapshot::ObjectRole::BackupBundle,backup_plain.len() as u64).unwrap();
        let key=risunest_external_storage_format::crypto::derive_key(&[7;32],&f.sender.descriptor.repository_id,"metadata").unwrap();
        let mut sealed=Vec::new();risunest_external_storage_format::snapshot::seal_envelope(&mut std::io::Cursor::new(backup_plain),&mut sealed,&key,&header).unwrap();
        f.provider.seed(&backup_object,ObjectRole::BackupBundle,sealed);
        assert_eq!(f.sender.snapshot_listing(&Cancellation::default()).await.unwrap().len(),1);
        // Restoring the backup finds it without opening the checkpoint beside it.
        let connected = super::cleanup::tests::fixture_connection(&f);
        let found = super::control::find_snapshot(&connected, backup_id, &Cancellation::default()).await.unwrap();
        assert_eq!(found.object_id, backup_object);
        let checkpoint_bytes = f.provider.contents(&snapshots[0].locator.object).unwrap();
        let open = |bytes: &[u8], receipt: ObjectReceipt, root_key: &[u8; 32]|
            super::lww_checkpoint::open(bytes, &receipt, &f.sender.repository, root_key).map(|_| ());
        open(&checkpoint_bytes, snapshots[0].clone(), &[7; 32]).unwrap();
        let mut damaged = checkpoint_bytes.clone();
        *damaged.last_mut().unwrap() ^= 1;
        assert_eq!(open(&damaged, snapshots[0].clone(), &[7; 32]).unwrap_err().kind, ErrorKind::Corrupt);
        let mut incomplete = snapshots[0].clone(); incomplete.complete = false;
        assert_eq!(open(&checkpoint_bytes, incomplete, &[7; 32]).unwrap_err().kind, ErrorKind::Corrupt);
        let mut short = snapshots[0].clone(); short.byte_length -= 1;
        assert_eq!(open(&checkpoint_bytes, short, &[7; 32]).unwrap_err().kind, ErrorKind::Corrupt);
        assert_eq!(open(&checkpoint_bytes, snapshots[0].clone(), &[8; 32]).unwrap_err().kind, ErrorKind::Corrupt);
        let checkpoint_key = risunest_external_storage_format::crypto::derive_key(&[7; 32], &snapshot.repository_id, "metadata").unwrap();
        let mut different_id = snapshot.clone();
        different_id.snapshot_id = "00000000-0000-4000-8000-000000000095".into();
        // A document that is whole for another repository is caught only by its
        // binding to the envelope it arrived in.
        let mut different_repository = snapshot.clone();
        different_repository.repository_id = "other-repository".into();
        for object in [&mut different_repository.library.record_catalog, &mut different_repository.library.asset_catalog]
            .into_iter().chain(different_repository.asset_catalogs.iter_mut()) {
            object.header.repository_id = "other-repository".into();
            object.ciphertext_length = risunest_external_storage_format::snapshot::envelope_length(&object.header).unwrap();
        }
        let different_repository = different_repository.encode().unwrap();
        for (plain, role) in [
            (b"{\"schema\":\"risunest.lww-snapshot/v1\"}".to_vec(), risunest_external_storage_format::snapshot::ObjectRole::SyncState),
            (different_id.encode().unwrap(), risunest_external_storage_format::snapshot::ObjectRole::SyncState),
            (different_repository, risunest_external_storage_format::snapshot::ObjectRole::SyncState),
            (snapshot.encode().unwrap(), risunest_external_storage_format::snapshot::ObjectRole::BackupBundle),
        ] {
            let header = risunest_external_storage_format::snapshot::PublicObjectHeader::new(
                snapshot.repository_id.clone(), snapshot.snapshot_id.clone(), role, plain.len() as u64,
            ).unwrap();
            let mut sealed = Vec::new();
            risunest_external_storage_format::snapshot::seal_envelope(&mut std::io::Cursor::new(plain), &mut sealed, &checkpoint_key, &header).unwrap();
            let mut receipt = snapshots[0].clone(); receipt.byte_length = sealed.len() as u64;
            assert_eq!(open(&sealed, receipt, &[7; 32]).unwrap_err().kind, ErrorKind::Corrupt);
        }
        let mut identical=snapshot.clone(); identical.snapshot_id="00000000-0000-4000-8000-000000000098".into();
        assert_eq!(super::lww_checkpoint::retained(&[snapshot.clone(),identical.clone()]).unwrap(),std::collections::BTreeSet::from([identical.snapshot_id.clone()]));
        let mut encoded:serde_json::Value=serde_json::from_slice(&snapshot.encode().unwrap()).unwrap();
        assert!(encoded["library"]["recordCatalog"]["header"]["plaintextLength"].is_string());
        encoded["library"]["recordCatalog"]["header"]["plaintextLength"]=serde_json::json!(0);
        assert!(super::lww_checkpoint::Checkpoint::decode(&serde_json::to_vec(&encoded).unwrap()).is_err());        let mut malformed_source=snapshot.clone();
        malformed_source.standalone_bodies.insert("a".repeat(64),super::lww_segment::LargeBody{object_id:"00000000-0000-4000-8000-000000000091".into(),sha256:"b".repeat(64),byte_length:DecimalU64(1),plaintext_byte_length:DecimalU64(1),locator:Some(completed.reference.receipt.locator.clone())});
        assert!(malformed_source.encode().is_err());        identical.state_identity="a".repeat(64);
        assert!(super::lww_checkpoint::retained(&[snapshot.clone(),identical]).is_err());
        for object in f.sender.listing(&Cancellation::default()).await.unwrap() {
            f.provider.delete_object(&f.sender.repository,&object.locator,&Cancellation::default()).await.unwrap();
        }
        assert_eq!(f.receive_b().await,1);
        assert_eq!(f.b.read_root(None).unwrap().value["language"],"ko");
        assert_eq!(f.a.read_root(None).unwrap().value["language"],"en");
        assert_eq!(f.receive_b().await,0);        let bad_id="00000000-0000-4000-8000-000000000092";
        let bad_plain=b"{\"schema\":\"risunest.lww-snapshot/v1\"}";
        let bad_header=risunest_external_storage_format::snapshot::PublicObjectHeader::new(snapshot.repository_id.clone(),bad_id.into(),risunest_external_storage_format::snapshot::ObjectRole::SyncState,bad_plain.len() as u64).unwrap();
        let mut bad_sealed=Vec::new();risunest_external_storage_format::snapshot::seal_envelope(&mut std::io::Cursor::new(bad_plain),&mut bad_sealed,&key,&bad_header).unwrap();
        f.provider.seed(bad_id,ObjectRole::Snapshot,bad_sealed);
        assert_eq!(f.receiver.snapshot_listing(&Cancellation::default()).await.unwrap_err().kind,ErrorKind::Corrupt);
        let (header,inspection)=binding_context(&f.b,&f.receiver,"corrupt-snapshot-binding");
        assert_eq!(f.receiver.stage_binding(&mut f.b,&header,&inspection,&Cancellation::default()).await.err().unwrap().kind,ErrorKind::Corrupt);
    }); }
}
#[test]
fn cycle_key_sets_match_real_emission_and_durable_apply_and_noop_is_empty() {
    use super::lww_engine::cycle_keys;
    run(async {
        let mut f = CycleFixture::new();
        cycle_keys::reset();
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        let published = f.publish_a().await;
        assert_eq!(f.receive_b().await, 1);
        let keys = cycle_keys::take();
        let expected = std::collections::BTreeSet::from([
            UnitKey::new(&["root", "language"]).unwrap(),
        ]);
        assert!(keys.complete());
        assert_eq!(keys.selected_keys, expected);
        assert_eq!(keys.attempted_keys, expected);
        assert_eq!(keys.emitted_keys, expected);
        assert_eq!(keys.affected_keys, expected);
        assert!(keys.held_keys.is_empty());
        assert!(keys.deferred_keys.is_empty());
        assert_eq!(keys.segment_attempts, 1);
        assert_eq!(keys.accepted_publications as u64, published.segments.0);
        assert_eq!(keys.receive_applies, 1);

        cycle_keys::reset();
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        assert_eq!(f.publish_a().await.segments.0, 0);
        assert_eq!(f.receive_b().await, 0);
        let keys = cycle_keys::take();
        assert!(keys.complete());
        assert!(keys.selected_keys.is_empty());
        assert!(keys.attempted_keys.is_empty());
        assert!(keys.emitted_keys.is_empty());
        assert!(keys.affected_keys.is_empty());
        assert_eq!(keys.segment_attempts, 0);
        assert_eq!(keys.accepted_publications, 0);
        assert_eq!(keys.receive_applies, 0);
    });
}
#[test]
fn cycle_key_sample_stays_failed_when_uncertain_publication_is_later_reconciled() {
    use super::lww_engine::cycle_keys;
    run(async {
        let mut f = CycleFixture::new();
        cycle_keys::reset();
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        f.provider.state.lock().unwrap().lose_response = true;
        assert!(f.sender.publish(&mut f.a, 0.into(), &[], &Cancellation::default()).await.is_err());
        assert_eq!(f.publish_a().await.segments.0, 1);
        assert_eq!(f.receive_b().await, 1);
        let keys = cycle_keys::take();
        assert!(!keys.complete());
        assert_eq!(keys.failed_operations, 1);
        assert_eq!(keys.pending_operations, 0);
        assert_eq!(keys.segment_attempts, 1);
        assert_eq!(keys.accepted_publications, 1);
        assert_eq!(keys.receive_applies, 1);
        assert_eq!(keys.selected_keys, keys.emitted_keys);
        assert_eq!(keys.attempted_keys, keys.emitted_keys);
        assert_eq!(keys.emitted_keys, keys.affected_keys);
        assert_eq!(keys.emitted_keys.len(), 1);
        assert_eq!(f.provider.upload_count(), 1);
    });
}
#[test]
fn cycle_key_sets_exclude_generating_publication_and_report_actual_deferred_apply() {
    use super::lww_engine::cycle_keys;
    run(async {
        let mut f = CycleFixture::new();
        conversation(&mut f.a);
        f.publish_a().await;
        f.receive_b().await;
        let generating = vec![MessageLocator {
            character_id: "char".into(), conversation_id: "conv".into(), start: None,
        }];
        cycle_keys::reset();
        f.a.commit(&WorkingSetCommit {
            expected_revision: f.a.revision().unwrap(),
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char".into(), conversation_id: "conv".into(),
                start: 0, delete_count: 0,
                messages: vec![serde_json::json!({"data":"synthetic-message","chatId":"synthetic-id"})],
                conversation: None, configured_index: None,
            }]),
            ..Default::default()
        }).unwrap();
        assert_eq!(f.sender.publish(&mut f.a, 0.into(), &generating, &Cancellation::default())
            .await.unwrap().segments.0, 0);
        let keys = cycle_keys::take();
        assert!(keys.complete());
        assert!(keys.selected_keys.is_empty());
        assert!(keys.attempted_keys.is_empty());
        assert!(keys.emitted_keys.is_empty());
        assert!(keys.affected_keys.is_empty());
        assert_eq!(keys.accepted_publications, 0);
        assert!(!f.a.lww_read_outbox(0.into(), 4096).unwrap().entries.is_empty());

        cycle_keys::reset();
        assert_eq!(f.publish_a().await.segments.0, 1);
        assert_eq!(f.receiver.receive_and_apply(&mut f.b, 0.into(), &generating,
            &Cancellation::default()).await.unwrap(), 1);
        let keys = cycle_keys::take();
        let expected = std::collections::BTreeSet::from([
            UnitKey::new(&["messages", "char", "conv"]).unwrap(),
        ]);
        assert!(keys.complete());
        assert_eq!(keys.selected_keys, expected);
        assert_eq!(keys.emitted_keys, expected);
        assert!(keys.affected_keys.is_empty());
        assert!(keys.held_keys.is_empty());
        assert_eq!(keys.deferred_keys, expected);
        assert_eq!(keys.receive_applies, 1);
    });
}
#[test]
fn crash_retry_uses_exact_ciphertext_nonce_sequence_and_capture() {
    run(async {
        let mut f = CycleFixture::new();
        set(&mut f.a, &["root", "language"], serde_json::json!("old"));
        f.provider.state.lock().unwrap().lose_response = true;
        assert!(f
            .sender
            .publish(&mut f.a, DecimalU64(0), &[], &Cancellation::default())
            .await
            .is_err());
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        let (pending, sealed) =
            f.a.external_lww_pending(&f.sender.target_scope(), &writer)
                .unwrap()
                .unwrap();
        assert!(pending.dispatched);
        assert_eq!(f.provider.contents(&pending.object_id).unwrap(), sealed);
        f.a = PersistentStore::open(f.directory_a.path()).unwrap();
        let header = crate::persistent_store::lww::Header {
            binding_authority: 0.into(),
            request_id: "initial-requeue-after-response-loss".into(),
        };
        f.a.lww_queue_unit_state_page(&header, None, 4096).unwrap();
        assert_eq!(f.publish_a().await.segments.0, 1);
        assert_eq!(f.provider.upload_attempts(&pending.object_id), 1);
        assert_eq!(f.provider.uploaded_ids(), vec![pending.object_id.clone()]);
        assert_eq!(f.provider.contents(&pending.object_id).unwrap(), sealed);
        assert!(f
            .a
            .lww_read_outbox(0.into(), 4096)
            .unwrap()
            .entries
            .is_empty());
        assert_eq!(
            f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer)
                .unwrap(),
            2
        );
        f.a.lww_queue_unit_state_page(&header, None, 4096).unwrap();
        assert!(f
            .a
            .lww_read_outbox(0.into(), 4096)
            .unwrap()
            .entries
            .is_empty());
        assert_eq!(f.publish_a().await.segments.0, 0);
        assert_eq!(f.provider.uploaded_ids(), vec![pending.object_id.clone()]);
        set(&mut f.a, &["root", "language"], serde_json::json!("new"));
        f.provider.state.lock().unwrap().lose_response = true;
        assert!(f
            .sender
            .publish(&mut f.a, 0.into(), &[], &Cancellation::default())
            .await
            .is_err());
        let (later_pending, later_sealed) =
            f.a.external_lww_pending(&f.sender.target_scope(), &writer)
                .unwrap()
                .unwrap();
        set(&mut f.a, &["root", "language"], serde_json::json!("newer"));
        f.a = PersistentStore::open(f.directory_a.path()).unwrap();
        f.a.lww_queue_unit_state_page(&header, None, 4096).unwrap();
        let newer = f.a.lww_read_outbox(0.into(), 4096).unwrap().entries;
        f.provider
            .fail_upload_number(f.provider.upload_count() + 1, ErrorKind::Transient);
        assert!(f
            .sender
            .publish(&mut f.a, 0.into(), &[], &Cancellation::default())
            .await
            .is_err());
        assert_eq!(f.a.lww_read_outbox(0.into(), 4096).unwrap().entries, newer);
        assert_eq!(f.provider.upload_attempts(&later_pending.object_id), 1);
        assert_eq!(
            f.provider.contents(&later_pending.object_id).unwrap(),
            later_sealed
        );
        assert_eq!(f.publish_a().await.segments.0, 1);
        assert_eq!(f.provider.upload_attempts(&pending.object_id), 1);
        assert_eq!(f.provider.contents(&pending.object_id).unwrap(), sealed);
        assert_eq!(
            f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer)
                .unwrap(),
            4
        );
        f.receive_b().await;
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "newer");
    })
}
#[test]
fn publisher_is_distinct_from_forwarded_unit_issuer() {
    run(async {
        let mut f = CycleFixture::new();
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        f.publish_a().await;
        f.receive_b().await;
        let issuer = f.a.lww_clock_state().unwrap().writer_id;
        let publisher = f.b.lww_clock_state().unwrap().writer_id;
        let before = f.b.lww_binding_state().unwrap();
        let target =
            crate::persistent_store::sync_selection::SyncTarget::External("forward".into());
        let inspection =
            f.b.register_lww_binding_inspection(
                before.target_authority,
                &target,
                "forward-target",
                "forward-library",
            )
            .unwrap();
        let state =
            f.b.switch_lww_binding(
                &crate::persistent_store::sync_selection::SwitchBindingRequest {
                    initial_publication: false,
                    header: crate::persistent_store::lww::Header {
                        binding_authority: before.target_authority,
                        request_id: "forward-bind".into(),
                    },
                    expected_selection_epoch: before.selection_epoch,
                    target,
                    inspection_id: Some(inspection),
                },
            )
            .unwrap();
        let header = crate::persistent_store::lww::Header {
            binding_authority: state.target_authority,
            request_id: "forward-existing".into(),
        };
        f.b.lww_queue_unit_state_page(&header, None, 100).unwrap();
        f.receiver
            .publish(
                &mut f.b,
                state.target_authority,
                &[],
                &Cancellation::default(),
            )
            .await
            .unwrap();
        let id = f
            .provider
            .uploaded_ids()
            .into_iter()
            .find(|id| parse_segment_object_id(id).unwrap().0 == publisher)
            .unwrap();
        let (_, seq, _) = parse_segment_object_id(&id).unwrap();
        let payload = lww_segment::open(
            &f.provider.contents(&id).unwrap(),
            &f.sender.library,
            &publisher,
            seq,
            &[7; 32],
        )
        .unwrap();
        assert!(payload
            .changes
            .iter()
            .any(|change| change.stamp.writer_id == issuer));
    })
}
#[test]
fn gaps_do_not_advance_and_authenticated_variants_stop_even_after_consumption() {
    run(async {
        let mut f = CycleFixture::new();
        set(&mut f.a, &["root", "language"], serde_json::json!("one"));
        f.publish_a().await;
        let first = f.provider.uploaded_ids()[0].clone();
        let bytes = f.provider.contents(&first).unwrap();
        f.provider.forget(&first);
        set(&mut f.a, &["root", "language"], serde_json::json!("two"));
        f.publish_a().await;
        assert_eq!(f.receive_b().await, 0);
        assert!(f.b.lww_receive_progress(DecimalU64(0)).unwrap().is_empty());
        f.provider.seed(&first, ObjectRole::Segment, bytes.clone());
        assert_eq!(f.receive_b().await, 2);
        let (writer, seq, _) = parse_segment_object_id(&first).unwrap();
        let mut payload =
            lww_segment::open(&bytes, &f.sender.library, writer, seq, &[7; 32]).unwrap();
        payload.changes.clear();
        let (variant, _) = lww_segment::seal(&payload, &[7; 32]).unwrap();
        let id = segment_object_id(writer, seq, &lww_segment::digest(&variant)).unwrap();
        f.provider.seed(&id, ObjectRole::Segment, variant);
        assert_eq!(
            f.receiver
                .receive_requests(&mut f.b, DecimalU64(0), &Cancellation::default())
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(f.provider.read_attempts(&id), 0);
    })
}
#[test]
fn generating_whole_messages_are_held_and_later_applied() {
    run(async {
        let mut f = CycleFixture::new();
        conversation(&mut f.a);
        f.publish_a().await;
        f.receive_b().await;
        f.a.commit(&WorkingSetCommit {
            expected_revision: f.a.revision().unwrap(),
            conversations: Some(vec![ConversationMutation::ReplaceRange {
                character_id: "char".into(),
                conversation_id: "conv".into(),
                start: 0,
                delete_count: 0,
                messages: vec![
                    serde_json::json!({"data":"synthetic-message","chatId":"synthetic-id"}),
                ],
                conversation: None,
                configured_index: None,
            }]),
            ..Default::default()
        })
        .unwrap();
        f.publish_a().await;
        let generating = vec![MessageLocator {
            character_id: "char".into(),
            conversation_id: "conv".into(),
            start: None,
        }];
        f.receiver
            .receive_and_apply(
                &mut f.b,
                DecimalU64(0),
                &generating,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        let header = crate::persistent_store::lww::Header {
            binding_authority: DecimalU64(0),
            request_id: "drain-generation".into(),
        };
        let result =
            f.b.lww_drain_deferred(&crate::persistent_store::lww::ApplyReceive {
                header,
                generating: vec![],
            })
            .unwrap();
        assert!(!result.affected_keys.is_empty());
    })
}
#[test]
fn large_bodies_stay_remote_then_only_missing_body_hydrates() {
    run(async {
        let mut f = CycleFixture::new();
        let body = vec![31; 5 * 1024 * 1024];
        let hash = risunest_sync_wire::hash(&body);
        f.a.lww_put_managed_object(&hash, &body).unwrap();
        let alias = crate::persistent_store::AssetAlias {
            key: "synthetic-asset".into(),
            object_hash: Some(hash.clone()),
            kind: "asset".into(),
            size: body.len() as i64,
            mime: "application/octet-stream".into(),
            name: "synthetic".into(),
            ext: "bin".into(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: serde_json::json!({}),
        };
        f.a.commit_asset_alias(&alias, f.a.revision().unwrap())
            .unwrap();
        f.publish_a().await;
        let second=vec![32;5*1024*1024];let second_hash=risunest_sync_wire::hash(&second);
        f.a.lww_put_managed_object(&second_hash,&second).unwrap();
        let mut second_alias=alias.clone();second_alias.key="synthetic-second-asset".into();second_alias.object_hash=Some(second_hash.clone());
        f.a.commit_asset_alias(&second_alias,f.a.revision().unwrap()).unwrap();f.publish_a().await;
        let inputs=tempfile::tempdir().unwrap();let mut state=f.receiver.published_state(&mut f.b,inputs.path(),&Cancellation::default()).await.unwrap();
        let first_root=state.standalone_roots.get(&hash).unwrap().clone();let second_root=state.standalone_roots.get(&second_hash).unwrap().clone();
        assert_ne!(first_root,second_root);
        for (body_hash,root) in [(&hash,&first_root),(&second_hash,&second_root)] {
            let (_,payload)=state.segments.iter().find(|(_,payload)|{let (writer,seq,_)=parse_segment_object_id(root).unwrap();payload.writer_id==writer&&payload.seq.0==seq}).unwrap();
            assert_eq!(payload.large_bodies.get(body_hash).unwrap().object_id,state.standalone.get(body_hash).unwrap().object_id);
        }
        f.receiver.stage_published_objects(&mut f.b,&mut state,inputs.path(),&Cancellation::default()).await.unwrap();
        assert_eq!(super::lww_residency::source(f.directory_b.path(),&hash).unwrap().unwrap().protected_segment,first_root);
        assert_eq!(super::lww_residency::source(f.directory_b.path(),&second_hash).unwrap().unwrap().protected_segment,second_root);        let before = f.provider.transferred_body_bytes();
        f.receive_b().await;
        assert!(!f.b.external_lww_object_is_local(&hash).unwrap());
        let source = super::lww_residency::source(f.directory_b.path(), &hash)
            .unwrap()
            .unwrap();
        assert!(parse_segment_object_id(&source.protected_segment).is_ok());
        assert_eq!(
            f.provider
                .read_attempts(&source.body.locator.as_ref().unwrap().object),
            0
        );
        assert_eq!(
            super::lww_residency::stat(f.directory_b.path(), &hash).unwrap(),
            Some(body.len() as u64)
        );
        let mut file = hydrate_receiver(&f, &hash).await;
        let mut restored = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut restored).unwrap();
        assert_eq!(restored, body);
        let hydrated = f.provider.transferred_body_bytes();
        assert!(hydrated.1 > before.1 + body.len() as u64);
        lww_segment::reset_hash_bytes();
        drop(hydrate_receiver(&f, &hash).await);
        assert_eq!(f.provider.transferred_body_bytes(), hydrated);
        assert_eq!(lww_segment::take_hash_bytes(), 0);
    })
}

fn binding_context(
    store: &PersistentStore,
    engine: &ExternalLwwEngine,
    request: &str,
) -> (crate::persistent_store::lww::Header, String) {
    let state = store.lww_binding_state().unwrap();
    let inspection = store
        .register_lww_binding_inspection(
            state.target_authority,
            &crate::persistent_store::sync_selection::SyncTarget::External(
                engine.connection_id.clone(),
            ),
            &engine.repository.connection_identity,
            &engine.library,
        )
        .unwrap();
    (
        crate::persistent_store::lww::Header {
            binding_authority: state.target_authority,
            request_id: request.into(),
        },
        inspection,
    )
}
#[test]
fn bootstrap_gap_blocks_activation_and_writer_authorization() {
    run(async {
        let mut f = CycleFixture::new();
        set(&mut f.a, &["root", "language"], serde_json::json!("one"));
        f.publish_a().await;
        let first = f.provider.uploaded_ids()[0].clone();
        f.provider.forget(&first);
        set(&mut f.a, &["root", "language"], serde_json::json!("two"));
        f.publish_a().await;
        let before = f.b.lww_clock_state().unwrap();
        let (header, inspection) = binding_context(&f.b, &f.receiver, "gap-binding");
        assert_eq!(
            f.receiver
                .stage_binding(&mut f.b, &header, &inspection, &Cancellation::default())
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::PreconditionFailed
        );
        assert_eq!(f.b.lww_clock_state().unwrap().writer_id, before.writer_id);
        assert_eq!(
            f.b.device_store()
                .unwrap()
                .connection()
                .query_row(
                    "SELECT count(*) FROM lww_new_device_authorizations",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    })
}
#[test]
fn coherent_new_device_uses_native_settlement_and_collided_target_never_authorizes() {
    run(async {
        let mut f = CycleFixture::new();
        set(&mut f.a, &["root", "language"], serde_json::json!("remote"));
        f.publish_a().await;
        set(
            &mut f.b,
            &["root", "language"],
            serde_json::json!("unsent-local"),
        );
        let before = f.b.lww_clock_state().unwrap();
        let (header, inspection) = binding_context(&f.b, &f.receiver, "new-device");
        let stage = f
            .receiver
            .stage_binding(&mut f.b, &header, &inspection, &Cancellation::default())
            .await
            .unwrap();
        let first = f.provider.uploaded_ids()[0].clone();
        let (writer, seq, _) = parse_segment_object_id(&first).unwrap();
        let mut payload = lww_segment::open(
            &f.provider.contents(&first).unwrap(),
            &f.sender.library,
            writer,
            seq,
            &[7; 32],
        )
        .unwrap();
        payload.changes.clear();
        let (variant, _) = lww_segment::seal(&payload, &[7; 32]).unwrap();
        let id = segment_object_id(writer, seq, &lww_segment::digest(&variant)).unwrap();
        f.provider.seed(&id, ObjectRole::Segment, variant);
        assert_eq!(
            f.receiver
                .prepare_new_device(
                    &mut f.b,
                    &header,
                    &stage.staging_id,
                    &Cancellation::default()
                )
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(f.b.lww_clock_state().unwrap().writer_id, before.writer_id);
        assert_eq!(
            f.b.lww_binding_authority().unwrap(),
            before.binding_authority
        );
        assert_eq!(
            f.b.read_root(None).unwrap().value["language"],
            "unsent-local"
        );
        assert_eq!(
            f.b.device_store()
                .unwrap()
                .connection()
                .query_row(
                    "SELECT count(*) FROM lww_new_device_authorizations",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        f.provider.forget(&id);
        let prepared = f
            .receiver
            .prepare_new_device(
                &mut f.b,
                &header,
                &stage.staging_id,
                &Cancellation::default(),
            )
            .await
            .unwrap();
        let result =
            f.b.lww_replace_target_as_new_device(
                &header,
                &stage.staging_id,
                &prepared.authorization_id,
            )
            .unwrap();
        assert_ne!(result.writer_id, before.writer_id);
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "remote");
        assert!(f
            .b
            .lww_read_outbox(result.binding_authority, 100)
            .unwrap()
            .entries
            .is_empty());
    })
}
#[test]
fn authenticated_future_stream_does_not_change_hlc_or_progress() {
    run(async {
        let mut f = CycleFixture::new();
        set(&mut f.a, &["root", "language"], serde_json::json!("future"));
        f.publish_a().await;
        let id = f.provider.uploaded_ids()[0].clone();
        let bytes = f.provider.contents(&id).unwrap();
        f.provider.forget(&id);
        let (writer, seq, _) = parse_segment_object_id(&id).unwrap();
        let mut payload =
            lww_segment::open(&bytes, &f.sender.library, writer, seq, &[7; 32]).unwrap();
        payload.changes[0].stamp.physical_ms = DecimalU64(super::runtime::now_ms() + 600_000);
        let (sealed, _) = lww_segment::seal(&payload, &[7; 32]).unwrap();
        let id = segment_object_id(writer, seq, &lww_segment::digest(&sealed)).unwrap();
        f.provider.seed(&id, ObjectRole::Segment, sealed);
        let before = f.b.lww_clock_state().unwrap();
        assert_eq!(
            f.receiver
                .receive_requests(&mut f.b, DecimalU64(0), &Cancellation::default())
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::ClockSkew
        );
        let after = f.b.lww_clock_state().unwrap();
        assert_eq!(after.accepted, before.accepted);
        assert_eq!(after.issued, before.issued);
        assert!(f.b.lww_receive_progress(DecimalU64(0)).unwrap().is_empty());
    })
}
#[test]
fn dependency_response_loss_reuses_exact_frozen_ciphertext_before_segment_seal() {
    run(async {
        let mut f = CycleFixture::new();
        let body = vec![33; 5 * 1024 * 1024];
        let hash = risunest_sync_wire::hash(&body);
        f.a.lww_put_managed_object(&hash, &body).unwrap();
        let alias = crate::persistent_store::AssetAlias {
            key: "synthetic".into(),
            object_hash: Some(hash.clone()),
            kind: "asset".into(),
            size: body.len() as i64,
            mime: "application/octet-stream".into(),
            name: "synthetic".into(),
            ext: "bin".into(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: serde_json::json!({}),
        };
        f.a.commit_asset_alias(&alias, f.a.revision().unwrap())
            .unwrap();
        let held = HeldAssetTransfer::new(f.provider.clone());
        held.lose_pack_response.store(true, std::sync::atomic::Ordering::SeqCst);
        held.cancel_after_pack_loss.store(true, std::sync::atomic::Ordering::SeqCst);
        f.sender.provider = held.clone();
        let reached = std::cell::Cell::new(false);
        let release = async { held.entered.notified().await; reached.set(true); held.resume.notify_one(); Ok::<_, ProviderError>(()) };
        let cancel = Cancellation::default();
        let publication = async {
            let result = f.sender.publish(&mut f.a, 0.into(), &[], &cancel).await;
            if !reached.get() { return Err(result.err().unwrap_or_else(|| ProviderError::new(ErrorKind::Corrupt))); }
            Ok(result)
        };
        let (result, ()) = tokio::try_join!(publication, release).unwrap();
        assert!(reached.get()); assert!(result.is_err());
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        let (pending, bytes) =
            f.a.external_lww_pending(&f.sender.target_scope(), &writer)
                .unwrap()
                .unwrap();
        assert!(!pending.sealed);
        assert!(bytes.is_empty());
        assert_eq!(pending.seq.0, 1);
        assert_eq!(pending.bodies.len(), 1);
        assert_eq!(pending.assets[0].content_hash, hash);
        assert_eq!(pending.assets[0].byte_length, body.len() as u64);
        assert_eq!(pending.bodies[0].content_hash, hash);
        assert!(!pending.bodies[0].sha256.is_empty());
        let draft_bytes = base64::Engine::decode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, &pending.payload).unwrap();
        let draft = lww_segment::Segment::decode_capture(&draft_bytes).unwrap();
        assert!(lww_segment::Segment::decode(&draft_bytes).is_err());
        assert!(draft.encode().is_err());
        assert!(lww_segment::seal(&draft, &f.sender.root_key).is_err());
        let original_job = pending.asset_job.clone();
        let object = pending.bodies[0].object_id.clone();
        let sealed = f.provider.contents(&object).unwrap();
        let original_intent = held.attempts.lock().unwrap()[0].clone();
        let mut changed_capture = pending.clone();
        changed_capture.assets[0].byte_length += 1;
        assert!(f.a.external_lww_persist(&changed_capture, &bytes).is_err());
        let mut changed_body = pending.clone();
        changed_body.bodies[0].byte_length += 1;
        assert!(f.a.external_lww_persist(&changed_body, &bytes).is_err());
        f.a = PersistentStore::open(f.directory_a.path()).unwrap();
        let reopened = f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap().0;
        assert!(reopened.asset_job == original_job);
        assert_eq!(reopened.assets, pending.assets);
        assert_eq!((&reopened.bodies[0].sha256, reopened.bodies[0].byte_length), (&pending.bodies[0].sha256, pending.bodies[0].byte_length));
        f.publish_a().await;
        assert_eq!(f.provider.contents(&object).unwrap(), sealed);
        assert!(held.attempts.lock().unwrap().iter().all(|intent| intent.object_id != object || intent.sha256 == original_intent.sha256));
        assert_eq!(
            f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer)
                .unwrap(),
            2
        );
        assert_eq!(
            f.provider
                .uploaded_ids()
                .iter()
                .filter(|id| parse_segment_object_id(id).is_ok())
                .count(),
            1
        );
    })
}

#[test]
fn switching_targets_uses_independent_sequences_and_ack_receipts_without_restamping() {
    run(async {
        let mut f = CycleFixture::new();
        set(
            &mut f.a,
            &["root", "language"],
            serde_json::json!("preserved"),
        );
        f.publish_a().await;
        let original =
            f.a.lww_read_unit_state(DecimalU64(0), None, 100)
                .unwrap()
                .entries;
        let before = f.a.lww_binding_state().unwrap();
        let target = crate::persistent_store::sync_selection::SyncTarget::External("next".into());
        f.sender.library = "new-library".into();
        let inspection =
            f.a.register_lww_binding_inspection(
                before.target_authority,
                &target,
                &f.sender.repository.connection_identity,
                &f.sender.library,
            )
            .unwrap();
        let bound =
            f.a.switch_lww_binding(
                &crate::persistent_store::sync_selection::SwitchBindingRequest {
                    initial_publication: false,
                    header: crate::persistent_store::lww::Header {
                        binding_authority: before.target_authority,
                        request_id: "different-target".into(),
                    },
                    expected_selection_epoch: before.selection_epoch,
                    target,
                    inspection_id: Some(inspection),
                },
            )
            .unwrap();
        let header = crate::persistent_store::lww::Header {
            binding_authority: bound.target_authority,
            request_id: "queue-target-initial".into(),
        };
        f.a.lww_queue_unit_state_page(&header, None, 100).unwrap();
        assert_eq!(
            f.sender
                .publish(
                    &mut f.a,
                    bound.target_authority,
                    &[],
                    &Cancellation::default()
                )
                .await
                .unwrap()
                .segments
                .0,
            1
        );
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        assert_eq!(
            f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer)
                .unwrap(),
            2
        );
        let after =
            f.a.lww_read_unit_state(bound.target_authority, None, 100)
                .unwrap()
                .entries;
        for prior in original {
            assert!(after
                .iter()
                .any(|entry| entry.key == prior.key && entry.stamp == prior.stamp));
        }
    })
}
#[test]
fn delegated_hash_observations_count_real_wire_invocations_without_rehashing() {
    use risunest_sync_wire::unit::UnitValue;
    let descriptor = risunest_sync_wire::descriptor::RecordDescriptor::content("a".repeat(64));
    let value = UnitValue::object(descriptor.clone()).unwrap();
    let bytes = descriptor.bytes().unwrap();
    let mut payload =
        lww_segment::Segment::new("library", "00000000-0000-4000-8000-000000000001", 1);
    payload.changes.push(crate::persistent_store::lww::Change {
        key: UnitKey::new(&["archive", "synthetic"]).unwrap(),
        stamp: risunest_sync_wire::stamp::Stamp {
            physical_ms: 1.into(),
            logical: 0,
            writer_id: payload.writer_id.clone(),
        },
        value: value.clone(),
    });
    lww_segment::reset_hash_bytes();
    lww_segment::reset_delegated_hash_work();
    payload.validate().unwrap();
    assert_eq!(lww_segment::take_hash_bytes(), 0);
    assert_eq!(lww_segment::take_hash_calls(), 0);
    assert_eq!(
        lww_segment::take_delegated_hash_work().get("segment-validate.descriptor"),
        Some(&(1, bytes.len() as u64))
    );
    let mut winners = std::collections::BTreeMap::new();
    lww_segment::merge(&mut winners, payload.changes[0].clone()).unwrap();
    lww_segment::merge(&mut winners, payload.changes[0].clone()).unwrap();
    let work = lww_segment::take_delegated_hash_work();
    assert_eq!(
        work["segment-merge.descriptor"],
        (2, 2 * bytes.len() as u64)
    );
    assert_eq!(
        work["segment-merge.identity"],
        (
            2,
            2 * risunest_sync_wire::canonical::encode(&value).unwrap().len() as u64
        )
    );
    assert_eq!(lww_segment::take_hash_bytes(), 0);
}
#[test]
fn production_cycle_reports_disjoint_native_and_transport_hash_work() {
    run(async {
        let mut fixture = CycleFixture::new();
        crate::persistent_store::hash_work::reset_hash_work();
        lww_segment::reset_hash_bytes();
        lww_segment::reset_delegated_hash_work();
        set(
            &mut fixture.a,
            &["root", "language"],
            serde_json::json!("counter"),
        );
        fixture.publish_a().await;
        fixture.receive_b().await;
        let native = crate::persistent_store::hash_work::take_hash_work();
        assert!(native.incomplete.is_empty());
        assert!(native.domains["native_unit_envelope"].calls > 0);
        assert!(lww_segment::take_hash_bytes() > 0);
        assert!(lww_segment::take_hash_calls() > 0);
        let delegated = lww_segment::take_delegated_hash_work();
        assert!(delegated["publish-ack.identity"].0 > 0);
        assert!(delegated["receive-history.identity"].0 > 0);
        assert!(!native.domains.contains_key("publish-ack.identity"));
        assert_eq!(
            fixture.b.read_root(None).unwrap().value["language"],
            "counter"
        );
    });
}

#[test]
fn superseded_published_values_retain_equal_stamp_integrity_history() {
    run(async {
        let mut f = CycleFixture::new();
        set(&mut f.a, &["root", "language"], serde_json::json!("first"));
        f.publish_a().await;
        let first = f.provider.uploaded_ids()[0].clone();
        let (writer, seq, _) = parse_segment_object_id(&first).unwrap();
        let mut payload = lww_segment::open(
            &f.provider.contents(&first).unwrap(),
            &f.sender.library,
            writer,
            seq,
            &[7; 32],
        )
        .unwrap();
        set(&mut f.a, &["root", "language"], serde_json::json!("later"));
        f.publish_a().await;
        payload.writer_id = f.b.lww_clock_state().unwrap().writer_id;
        payload.seq = 1.into();
        payload.changes[0].value =
            risunest_sync_wire::unit::UnitValue::inline(br#""changed""#).unwrap();
        let (bytes, _) = lww_segment::seal(&payload, &[7; 32]).unwrap();
        let id = segment_object_id(&payload.writer_id, 1, &lww_segment::digest(&bytes)).unwrap();
        f.provider.seed(&id, ObjectRole::Segment, bytes);
        let before = f.a.lww_clock_state().unwrap();
        assert_eq!(
            f.sender
                .receive_requests(&mut f.a, DecimalU64(0), &Cancellation::default())
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
        assert_eq!(f.a.lww_clock_state().unwrap().accepted, before.accepted);
        assert_eq!(f.a.read_root(None).unwrap().value["language"], "later");
    })
}
#[test]
fn native_unpublished_clock_repair_preserves_values_and_never_repairs_dispatched_bytes() {
    run(async {
        let mut f = CycleFixture::new();
        let future = risunest_sync_wire::stamp::Stamp {
            physical_ms: DecimalU64(super::runtime::now_ms() + 1_000_000),
            logical: 0,
            writer_id: f.a.lww_clock_state().unwrap().writer_id,
        };
        f.a.device_store()
            .unwrap()
            .connection()
            .execute(
                "UPDATE lww_clock SET issued=?1",
                [serde_json::to_string(&future).unwrap()],
            )
            .unwrap();
        set(
            &mut f.a,
            &["root", "language"],
            serde_json::json!("same-value"),
        );
        let original = f.a.lww_read_outbox(0.into(), 100).unwrap().entries;
        f.a.device_store().unwrap().connection().execute("UPDATE lww_clock SET accepted=?1",
            [serde_json::to_string(&future).unwrap()]).unwrap();
        let error = f.sender.publish(&mut f.a, 0.into(), &[], &Cancellation::default()).await.err().unwrap();
        assert_eq!(error.kind, ErrorKind::ClockSkew);
        assert_eq!(serde_json::to_value(&error).unwrap()["detail"],
            "stage=publish reason=clock-repair-failed repair_error=accepted-clock-correction-required");
        f.a.device_store().unwrap().connection().execute("UPDATE lww_clock SET accepted=NULL", []).unwrap();
        f.a.device_store().unwrap().connection().execute_batch(
            "CREATE TEMP TRIGGER refuse_clock_repair BEFORE UPDATE ON lww_clock
             BEGIN SELECT RAISE(ABORT, 'synthetic-private-value'); END;").unwrap();
        let error = f.sender.publish(&mut f.a, 0.into(), &[], &Cancellation::default()).await.err().unwrap();
        assert_eq!(serde_json::to_value(&error).unwrap()["detail"],
            "stage=publish reason=clock-repair-failed repair_error=store-error");
        f.a.device_store().unwrap().connection().execute_batch("DROP TRIGGER refuse_clock_repair").unwrap();
        assert_eq!(f.a.lww_read_outbox(0.into(), 100).unwrap().entries, original);
        f.a.device_store_mut().unwrap().write_plugin_device_values("inactive", &[crate::persistent_store::device_store::plugin_values::PluginDeviceMutation::Set {space:"string".into(),key:"synthetic".into(),value:"kept".into()}]).unwrap();
        f.publish_a().await;
        assert_eq!(f.a.read_root(None).unwrap().value["language"], "same-value");
        assert!(
            f.a.lww_clock_state().unwrap().issued.unwrap().physical_ms
                < original[0].stamp.physical_ms
        );
        set(
            &mut f.a,
            &["root", "language"],
            serde_json::json!("dispatched"),
        );
        f.provider.state.lock().unwrap().lose_response = true;
        assert!(f
            .sender
            .publish(&mut f.a, 0.into(), &[], &Cancellation::default())
            .await
            .is_err());
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        let (pending, bytes) =
            f.a.external_lww_pending(&f.sender.target_scope(), &writer)
                .unwrap()
                .unwrap();
        // A trusted time discontinuity cannot authorize rewriting an already dispatched intent.
        f.sender.invalidate_clock();
        assert_eq!(
            f.sender
                .publish(&mut f.a, 0.into(), &[], &Cancellation::default())
                .await
                .err()
                .unwrap()
                .kind,
            ErrorKind::ClockSkew
        );
        let (after, after_bytes) =
            f.a.external_lww_pending(&f.sender.target_scope(), &writer)
                .unwrap()
                .unwrap();
        assert_eq!(after.object_id, pending.object_id);
        assert_eq!(after_bytes, bytes);
    })
}

fn bind_external(store: &mut PersistentStore, engine: &ExternalLwwEngine, target: &crate::persistent_store::sync_selection::SyncTarget) -> DecimalU64 {
    use crate::persistent_store::sync_selection::SwitchBindingRequest;
    let before = store.lww_binding_state().unwrap();
    let inspection_id = match target {
        crate::persistent_store::sync_selection::SyncTarget::None => None,
        _ => Some(store.register_lww_binding_inspection(before.target_authority, target, &engine.repository.connection_identity, &engine.library).unwrap()),
    };
    store.switch_lww_binding(&SwitchBindingRequest {
        initial_publication: false,
        header: crate::persistent_store::lww::Header { binding_authority: before.target_authority, request_id: uuid::Uuid::new_v4().to_string() },
        expected_selection_epoch: before.selection_epoch, target: target.clone(), inspection_id,
    }).unwrap().target_authority
}
#[test]
fn rebinding_the_same_repository_finishes_the_sealed_segment_and_publishes_offline_edits() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let target = SyncTarget::External("repository".into());
        let first = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "language"], serde_json::json!("ja"));
        f.sender.publish(&mut f.a, first, &[], &Cancellation::default()).await.unwrap();
        f.receive_b().await;
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "ja");
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        let transfer = HeldAssetTransfer::new(f.provider.clone());
        transfer.held.store(false, std::sync::atomic::Ordering::SeqCst);
        transfer.fail_segment_begin.store(true, std::sync::atomic::Ordering::SeqCst);
        f.sender.provider = transfer;
        assert_eq!(f.sender.publish(&mut f.a, first, &[], &Cancellation::default()).await.err().unwrap().kind, ErrorKind::Transient);
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        let pending = f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap().0;
        assert!(pending.sealed && !pending.dispatched);
        f.sender.provider = f.provider.clone();
        bind_external(&mut f.a, &f.sender, &SyncTarget::None);
        set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
        let rebound = bind_external(&mut f.a, &f.sender, &target);
        let published = f.sender.publish(&mut f.a, rebound, &[], &Cancellation::default()).await.unwrap();
        assert_eq!(published.segments.0, 2);
        assert!(f.a.lww_read_outbox(rebound, 100).unwrap().entries.is_empty());
        assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), pending.seq.0 + 2);
        f.receive_b().await;
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "ko");
        assert_eq!(f.b.read_root(None).unwrap().value["askRemoval"], true);
    })
}
#[test]
fn a_rebind_drops_an_unfinished_receive_and_receives_its_segment_again() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let target = SyncTarget::External("repository".into());
        let sent = bind_external(&mut f.a, &f.sender, &target);
        let bound = bind_external(&mut f.b, &f.receiver, &target);
        set(&mut f.a, &["root", "language"], serde_json::json!("ja"));
        f.sender.publish(&mut f.a, sent, &[], &Cancellation::default()).await.unwrap();
        let requests = f.receiver.receive_requests(&mut f.b, bound, &Cancellation::default()).await.unwrap();
        assert_eq!(requests.len(), 1);
        let unfinished = requests[0].header.request_id.clone();
        f.b.lww_stage_receive(&requests[0]).unwrap();
        f.b.lww_apply_receive(&crate::persistent_store::lww::ApplyReceive { header: requests[0].header.clone(), generating: vec![] }).unwrap();
        assert_ne!(f.b.lww_receive_row_counts(&unfinished).unwrap().0, 0);
        bind_external(&mut f.b, &f.receiver, &SyncTarget::None);
        assert_eq!(f.b.lww_receive_row_counts(&unfinished).unwrap(), (0, 0));
        let rebound = bind_external(&mut f.b, &f.receiver, &target);
        assert_eq!(f.receiver.receive_and_apply(&mut f.b, rebound, &[], &Cancellation::default()).await.unwrap(), 1);
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "ja");
        assert_eq!(f.b.lww_receive_row_counts(&unfinished).unwrap(), (0, 0));
    })
}
/// The fake repository behind a connection that can fail every request with
/// one kind of error, or lose the answer to the next segment upload.
struct FlakyRemote {
    inner: Arc<FakeProvider>,
    failure: std::sync::Mutex<Option<ErrorKind>>,
    lost_segment: std::sync::Mutex<Option<bool>>,
}
impl FlakyRemote {
    fn new(inner: Arc<FakeProvider>) -> Arc<Self> {
        Arc::new(Self { inner, failure: Default::default(), lost_segment: Default::default() })
    }
    fn fail(&self, failure: Option<ErrorKind>) {
        *self.failure.lock().unwrap() = failure;
    }
    fn check(&self) -> Result<()> {
        match *self.failure.lock().unwrap() {
            Some(kind) => Err(ProviderError::new(kind)),
            None => Ok(()),
        }
    }
}
impl Provider for FlakyRemote {
    fn open_repository<'a>(&'a self, config: &'a ConnectionConfig, secret: &'a SecretRef, mode: OpenMode, cancel: &'a Cancellation)
        -> ProviderFuture<'a, (RepositoryHandle, super::capabilities::Capabilities)> {
        Box::pin(async move { self.check()?; self.inner.open_repository(config, secret, mode, cancel).await })
    }
    fn read_object<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, unchanged: Option<&'a VersionToken>, sink: &'a mut dyn TransferSink, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ReadReceipt> {
        Box::pin(async move { self.check()?; self.inner.read_object(repository, locator, unchanged, sink, cancel).await })
    }
    fn begin_upload<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, cancel: &'a Cancellation)
        -> ProviderFuture<'a, Option<ResumeState>> {
        Box::pin(async move { self.check()?; self.inner.begin_upload(repository, intent, cancel).await })
    }
    fn create_object<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, source: &'a dyn TransferSource, resume: Option<&'a ResumeState>, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ObjectReceipt> {
        Box::pin(async move {
            self.check()?;
            let lost = if intent.role == ObjectRole::Segment { self.lost_segment.lock().unwrap().take() } else { None };
            let Some(landed) = lost else {
                return self.inner.create_object(repository, intent, source, resume, cancel).await;
            };
            if landed {
                self.inner.create_object(repository, intent, source, resume, cancel).await?;
            }
            Err(ProviderError::new(ErrorKind::Transient))
        })
    }
    fn compare_exchange_head<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, expected: &'a ExpectedHead, head: &'a HeadBytes, cancel: &'a Cancellation)
        -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move { self.check()?; self.inner.compare_exchange_head(repository, locator, expected, head, cancel).await })
    }
    fn replace_head<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, head: &'a HeadBytes, cancel: &'a Cancellation)
        -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move { self.check()?; self.inner.replace_head(repository, locator, head, cancel).await })
    }
    fn list_objects<'a>(&'a self, repository: &'a RepositoryHandle, collection: Collection, cursor: Option<&'a str>, limit: u16, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ObjectPage> {
        Box::pin(async move { self.check()?; self.inner.list_objects(repository, collection, cursor, limit, cancel).await })
    }
    fn delete_object<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ()> {
        Box::pin(async move { self.check()?; self.inner.delete_object(repository, locator, cancel).await })
    }
    fn reconcile_upload<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, resume: Option<&'a ResumeState>, cancel: &'a Cancellation)
        -> ProviderFuture<'a, UploadResolution> {
        Box::pin(async move { self.check()?; self.inner.reconcile_upload(repository, intent, resume, cancel).await })
    }
    fn lookup_metadata<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, known: Option<&'a RemoteLocator>, cancel: &'a Cancellation)
        -> ProviderFuture<'a, Option<ObjectReceipt>> {
        Box::pin(async move { self.check()?; self.inner.lookup_metadata(repository, intent, known, cancel).await })
    }
    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> { self.inner.head_locator(repository) }
}
/// Uploads large bodies through a session, as S3 multipart and OneDrive
/// upload sessions do. The first transfer lands without an answer, and the
/// finished session then refuses every later fragment.
struct SessionRemote {
    inner: Arc<FakeProvider>,
    landed: std::sync::Mutex<bool>,
}
impl Provider for SessionRemote {
    fn open_repository<'a>(&'a self, config: &'a ConnectionConfig, secret: &'a SecretRef, mode: OpenMode, cancel: &'a Cancellation)
        -> ProviderFuture<'a, (RepositoryHandle, super::capabilities::Capabilities)> {
        self.inner.open_repository(config, secret, mode, cancel)
    }
    fn read_object<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, unchanged: Option<&'a VersionToken>, sink: &'a mut dyn TransferSink, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ReadReceipt> {
        self.inner.read_object(repository, locator, unchanged, sink, cancel)
    }
    fn begin_upload<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, cancel: &'a Cancellation)
        -> ProviderFuture<'a, Option<ResumeState>> {
        Box::pin(async move {
            if intent.role != ObjectRole::Pack { return self.inner.begin_upload(repository, intent, cancel).await; }
            Ok(Some(ResumeState { data: SecretRef("synthetic-session".into()).into(), confirmed_offset: 0, expires_at_ms: None }))
        })
    }
    fn create_object<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, source: &'a dyn TransferSource, resume: Option<&'a ResumeState>, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ObjectReceipt> {
        Box::pin(async move {
            if resume.is_none() { return self.inner.create_object(repository, intent, source, None, cancel).await; }
            let landed = std::mem::replace(&mut *self.landed.lock().unwrap(), true);
            if landed { return Err(ProviderError::new(ErrorKind::NotFound)); }
            self.inner.create_object(repository, intent, source, None, cancel).await?;
            Err(ProviderError::new(ErrorKind::Transient))
        })
    }
    fn compare_exchange_head<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, expected: &'a ExpectedHead, head: &'a HeadBytes, cancel: &'a Cancellation)
        -> ProviderFuture<'a, HeadReceipt> {
        self.inner.compare_exchange_head(repository, locator, expected, head, cancel)
    }
    fn replace_head<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, head: &'a HeadBytes, cancel: &'a Cancellation)
        -> ProviderFuture<'a, HeadReceipt> {
        self.inner.replace_head(repository, locator, head, cancel)
    }
    fn list_objects<'a>(&'a self, repository: &'a RepositoryHandle, collection: Collection, cursor: Option<&'a str>, limit: u16, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ObjectPage> {
        self.inner.list_objects(repository, collection, cursor, limit, cancel)
    }
    fn delete_object<'a>(&'a self, repository: &'a RepositoryHandle, locator: &'a RemoteLocator, cancel: &'a Cancellation)
        -> ProviderFuture<'a, ()> {
        self.inner.delete_object(repository, locator, cancel)
    }
    fn reconcile_upload<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, resume: Option<&'a ResumeState>, cancel: &'a Cancellation)
        -> ProviderFuture<'a, UploadResolution> {
        self.inner.reconcile_upload(repository, intent, resume, cancel)
    }
    fn lookup_metadata<'a>(&'a self, repository: &'a RepositoryHandle, intent: &'a ObjectIntent, known: Option<&'a RemoteLocator>, cancel: &'a Cancellation)
        -> ProviderFuture<'a, Option<ObjectReceipt>> {
        self.inner.lookup_metadata(repository, intent, known, cancel)
    }
    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> { self.inner.head_locator(repository) }
}

#[test]
fn a_large_body_whose_finished_session_lost_its_answer_publishes_on_the_next_attempt() {
    run(async {
        let mut f = CycleFixture::new();
        let body = vec![71; 5 * 1024 * 1024];
        let hash = small_asset(&mut f.a, "synthetic-session-body", &body);
        f.sender.provider = Arc::new(SessionRemote { inner: f.provider.clone(), landed: Default::default() });
        let cancel = Cancellation::default();
        let lost = f.sender.publish(&mut f.a, DecimalU64(0), &[], &cancel).await;
        assert_eq!(lost.err().unwrap().kind, ErrorKind::Transient);
        f.sender.publish(&mut f.a, DecimalU64(0), &[], &cancel).await.unwrap();
        f.sender.provider = f.provider.clone();
        assert_eq!(f.provider.objects_with_role(ObjectRole::Pack).len(), 1);
        f.receive_b().await;
        let inputs = tempfile::tempdir().unwrap();
        let state = f.receiver.published_state(&mut f.b, inputs.path(), &cancel).await.unwrap();
        assert!(state.standalone.contains_key(&hash));
    });
}

/// Leaves the next segment sent without an answer: `landed` decides whether
/// the repository kept it.
async fn send_unconfirmed(f: &mut CycleFixture, authority: DecimalU64, landed: bool) -> crate::persistent_store::external_lww::SealedPublication {
    let remote = FlakyRemote::new(f.provider.clone());
    *remote.lost_segment.lock().unwrap() = Some(landed);
    f.sender.provider = remote;
    let sent = f.sender.publish(&mut f.a, authority, &[], &Cancellation::default()).await;
    f.sender.provider = f.provider.clone();
    assert_eq!(sent.err().unwrap().kind, ErrorKind::Transient);
    let writer = f.a.lww_clock_state().unwrap().writer_id;
    let pending = f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap().0;
    assert!(pending.dispatched && !pending.complete);
    assert_eq!(f.provider.holds(&pending.object_id), landed);
    pending
}
/// Leaves the next segment sealed but never sent.
async fn seal_unsent(f: &mut CycleFixture, authority: DecimalU64) -> crate::persistent_store::external_lww::SealedPublication {
    let transfer = HeldAssetTransfer::new(f.provider.clone());
    transfer.held.store(false, std::sync::atomic::Ordering::SeqCst);
    transfer.fail_segment_begin.store(true, std::sync::atomic::Ordering::SeqCst);
    f.sender.provider = transfer;
    let sent = f.sender.publish(&mut f.a, authority, &[], &Cancellation::default()).await;
    f.sender.provider = f.provider.clone();
    assert_eq!(sent.err().unwrap().kind, ErrorKind::Transient);
    let writer = f.a.lww_clock_state().unwrap().writer_id;
    let pending = f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap().0;
    assert!(pending.sealed && !pending.dispatched);
    pending
}
/// Binds another repository, so the next binding of the fixture repository
/// starts without the pending state of the earlier one.
fn bind_elsewhere(store: &mut PersistentStore) -> DecimalU64 {
    use crate::persistent_store::sync_selection::{SwitchBindingRequest, SyncTarget};
    let target = SyncTarget::External("elsewhere".into());
    let before = store.lww_binding_state().unwrap();
    let inspection_id = store.register_lww_binding_inspection(before.target_authority, &target, "synthetic-account/elsewhere", "synthetic-elsewhere-library").unwrap();
    store.switch_lww_binding(&SwitchBindingRequest {
        initial_publication: false,
        header: crate::persistent_store::lww::Header { binding_authority: before.target_authority, request_id: uuid::Uuid::new_v4().to_string() },
        expected_selection_epoch: before.selection_epoch, target, inspection_id: Some(inspection_id),
    }).unwrap().target_authority
}
#[test]
fn unbinding_from_an_unreachable_repository_does_not_wait_for_a_sent_segment() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let target = SyncTarget::External("repository".into());
        let first = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        assert!(!f.a.external_lww_unconfirmed_dispatch().unwrap());
        let pending = send_unconfirmed(&mut f, first, false).await;
        assert!(f.a.external_lww_unconfirmed_dispatch().unwrap());
        let remote = FlakyRemote::new(f.provider.clone());
        remote.fail(Some(ErrorKind::Transient));
        f.sender.provider = remote;
        let fenced = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            f.sender.fence_binding_change(&mut f.a, false, &Cancellation::default()),
        ).await.expect("the fence must not wait on the repository");
        assert_eq!(fenced, Ok(()));
        bind_external(&mut f.a, &f.sender, &SyncTarget::None);
        assert!(f.a.external_lww_unconfirmed_dispatch().unwrap());
        let rebound = bind_external(&mut f.a, &f.sender, &target);
        f.sender.provider = f.provider.clone();
        assert_eq!(f.sender.publish(&mut f.a, rebound, &[], &Cancellation::default()).await.unwrap().segments.0, 1);
        assert!(!f.a.external_lww_unconfirmed_dispatch().unwrap());
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), pending.seq.0 + 1);
        assert!(f.a.lww_read_outbox(rebound, 100).unwrap().entries.is_empty());
        f.receive_b().await;
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "ko");
    })
}
#[test]
fn a_binding_fence_stops_only_for_a_refusal_or_a_damaged_segment_and_passes_a_missing_repository() {
    run(async {
        let mut f = CycleFixture::new();
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        let pending = send_unconfirmed(&mut f, DecimalU64(0), false).await;
        let remote = FlakyRemote::new(f.provider.clone());
        f.sender.provider = remote.clone();
        let cancel = Cancellation::default();
        for (kind, passes, passes_new_device) in [
            (ErrorKind::Transient, true, true),
            (ErrorKind::RateLimited, true, true),
            (ErrorKind::DailyQuotaExhausted, true, true),
            (ErrorKind::EndpointRejected, true, true),
            (ErrorKind::Unauthorized, false, true),
            (ErrorKind::ReauthRequired, false, true),
            (ErrorKind::NotFound, true, true),
            (ErrorKind::RepositoryKeyUnavailable, false, false),
            (ErrorKind::Corrupt, false, false),
            (ErrorKind::Cancelled, false, false),
        ] {
            remote.fail(Some(kind));
            for (new_device, passes) in [(false, passes), (true, passes_new_device)] {
                let fenced = f.sender.fence_binding_change(&mut f.a, new_device, &cancel).await.map_err(|error| error.kind);
                assert_eq!(fenced, if passes { Ok(()) } else { Err(kind) }, "{kind:?}, new device: {new_device}");
            }
        }
        remote.fail(None);
        assert_eq!(f.sender.fence_binding_change(&mut f.a, false, &cancel).await, Ok(()));
        assert_eq!(f.sender.settle_publication(&mut f.a, &cancel).await.err().unwrap().kind, ErrorKind::PreconditionFailed);
        f.provider.seed(&pending.object_id, ObjectRole::Segment, b"synthetic other segment".to_vec());
        for new_device in [false, true] {
            assert_eq!(f.sender.fence_binding_change(&mut f.a, new_device, &cancel).await.err().unwrap().kind, ErrorKind::Corrupt);
        }
    })
}
#[test]
fn returning_to_a_repository_without_its_pending_state_drops_an_unsent_segment() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let target = SyncTarget::External("repository".into());
        let first = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "language"], serde_json::json!("ja"));
        f.sender.publish(&mut f.a, first, &[], &Cancellation::default()).await.unwrap();
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        let pending = seal_unsent(&mut f, first).await;
        assert!(!f.a.external_lww_unconfirmed_dispatch().unwrap());
        bind_elsewhere(&mut f.a);
        let rebound = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
        assert_eq!(f.sender.publish(&mut f.a, rebound, &[], &Cancellation::default()).await.unwrap().segments.0, 1);
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), pending.seq.0 + 1);
        assert_eq!(f.provider.upload_attempts(&pending.object_id), 0);
        f.receive_b().await;
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "ja");
        assert_eq!(f.b.read_root(None).unwrap().value["askRemoval"], true);
    })
}
#[test]
fn returning_to_a_repository_without_its_pending_state_counts_a_landed_segment_without_sending_it_again() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let target = SyncTarget::External("repository".into());
        let first = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        let pending = send_unconfirmed(&mut f, first, true).await;
        assert_eq!(f.sender.fence_binding_change(&mut f.a, false, &Cancellation::default()).await, Ok(()));
        assert!(f.a.external_lww_unconfirmed_dispatch().unwrap());
        bind_elsewhere(&mut f.a);
        assert!(!f.a.external_lww_unconfirmed_dispatch().unwrap());
        let rebound = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
        assert_eq!(f.sender.publish(&mut f.a, rebound, &[], &Cancellation::default()).await.unwrap().segments.0, 1);
        assert_eq!(f.provider.upload_attempts(&pending.object_id), 1);
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), pending.seq.0 + 2);
        f.sender.receive_and_apply(&mut f.a, rebound, &[], &Cancellation::default()).await.unwrap();
        assert_eq!(f.a.read_root(None).unwrap().value["language"], "ko");
        assert_eq!(f.a.read_root(None).unwrap().value["askRemoval"], true);
        f.receive_b().await;
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "ko");
        assert_eq!(f.b.read_root(None).unwrap().value["askRemoval"], true);
    })
}
#[test]
fn an_owed_initial_publication_reaches_the_repository_after_a_stop() {
    run(async {
        use crate::persistent_store::sync_selection::{SwitchBindingRequest, SyncTarget};
        let mut f = CycleFixture::new();
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
        let before = f.a.lww_binding_state().unwrap();
        let target = SyncTarget::External("repository".into());
        let inspection_id = f.a.register_lww_binding_inspection(before.target_authority, &target, &f.sender.repository.connection_identity, &f.sender.library).unwrap();
        let authority = f.a.switch_lww_binding(&SwitchBindingRequest {
            header: crate::persistent_store::lww::Header { binding_authority: before.target_authority, request_id: uuid::Uuid::new_v4().to_string() },
            expected_selection_epoch: before.selection_epoch, target, inspection_id: Some(inspection_id), initial_publication: true,
        }).unwrap().target_authority;
        let header = crate::persistent_store::lww::Header { binding_authority: authority, request_id: "synthetic-initial".into() };
        // Stops between the library and device commits of a page, and between
        // pages, each followed by a restart.
        for commits in [1, 2, 3] {
            f.a.stop_initial_queue_after_commits(1, commits);
            assert!(f.a.lww_finish_initial_publication(&header).is_err());
            f.restart_a();
            assert!(f.a.lww_owed_initial_publication().unwrap().is_some());
        }
        f.a.stop_initial_queue_after_commits(1, usize::MAX);
        assert!(f.a.lww_finish_initial_publication(&header).unwrap());
        assert_eq!(f.a.lww_owed_initial_publication().unwrap(), None);
        assert!(!f.a.lww_finish_initial_publication(&header).unwrap());
        f.sender.publish(&mut f.a, authority, &[], &Cancellation::default()).await.unwrap();
        assert!(f.a.lww_read_outbox(authority, 100).unwrap().entries.is_empty());
        f.receive_b().await;
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "ko");
        assert_eq!(f.b.read_root(None).unwrap().value["askRemoval"], true);
    })
}
fn job_released(store: &PersistentStore, job: &crate::external_storage::journal::JobIdentity) -> bool {
    match crate::asset_repository::job_pins::DurableCasJob::open(store.repository_root(), &job.job_id) {
        Ok(_) => false,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}
fn writer_rows(store: &PersistentStore, table: &str, writer: &str) -> i64 {
    store.device_store().unwrap().connection()
        .query_row(&format!("SELECT count(*) FROM {table} WHERE writer=?1"), [writer], |r| r.get(0))
        .unwrap()
}
#[test]
fn a_new_device_switch_drops_the_old_writers_segments_and_releases_their_files() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let target = SyncTarget::External("repository".into());
        let first = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "language"], serde_json::json!("ja"));
        f.sender.publish(&mut f.a, first, &[], &Cancellation::default()).await.unwrap();
        small_asset(&mut f.a, "synthetic-asset", b"synthetic old writer asset");
        let pending = send_unconfirmed(&mut f, first, false).await;
        let job = pending.asset_job.clone().expect("the segment holds its asset");
        let old_writer = f.a.lww_clock_state().unwrap().writer_id;
        // The finished publication left no row; the unconfirmed one keeps its row and files.
        assert_eq!(writer_rows(&f.a, "external_lww_segments", &old_writer), 1);
        assert_eq!(writer_rows(&f.a, "external_lww_sequences", &old_writer), 1);
        let files = crate::persistent_store::external_lww::publication_directory(f.directory_a.path(), &job.job_id);
        assert!(files.exists());
        bind_elsewhere(&mut f.a);
        let (header, inspection) = binding_context(&f.a, &f.sender, "synthetic-new-device");
        let stage = f.sender.stage_binding(&mut f.a, &header, &inspection, &Cancellation::default()).await.unwrap();
        let preparation = f.a.prepare_lww_new_device(&header, &stage.staging_id).unwrap();
        f.a.authorize_lww_new_device(&preparation.authorization_id).unwrap();
        let result = f.a.lww_replace_target_as_new_device(&header, &stage.staging_id, &preparation.authorization_id).unwrap();
        assert_ne!(result.writer_id, old_writer);
        assert_eq!(writer_rows(&f.a, "external_lww_segments", &old_writer), 0);
        assert_eq!(writer_rows(&f.a, "external_lww_sequences", &old_writer), 0);
        assert!(job_released(&f.a, &job));
        assert!(!files.exists(), "the retired writer's publication files stay");
    })
}
#[test]
fn fresh_writer_adoption_retries_retired_job_cleanup_and_preserves_other_writers() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let authority = bind_external(&mut f.a, &f.sender, &SyncTarget::External("repository".into()));
        let old_writer = f.a.lww_clock_state().unwrap().writer_id;
        let other_writer = uuid::Uuid::new_v4().to_string();
        let library = f.sender.library.clone();
        // Seed another writer's real pending job without retiring its local work.
        f.a.device_store().unwrap().connection().execute(
            "UPDATE device_meta SET writer_id=?1 WHERE singleton=1", [&other_writer],
        ).unwrap();
        f.sender.library = "synthetic-other-library".into();
        set(&mut f.a, &["root", "language"], serde_json::json!("other-sequence"));
        f.sender.publish(&mut f.a, authority, &[], &Cancellation::default()).await.unwrap();
        small_asset(&mut f.a, "synthetic-other-asset", b"synthetic unrelated writer asset");
        let other = seal_unsent(&mut f, authority).await;
        let other_job = other.asset_job.clone().unwrap();
        let other_files = crate::persistent_store::external_lww::publication_directory(f.directory_a.path(), &other_job.job_id);
        f.a.device_store().unwrap().connection().execute(
            "UPDATE device_meta SET writer_id=?1 WHERE singleton=1", [&old_writer],
        ).unwrap();
        f.sender.library = library;
        set(&mut f.a, &["root", "language"], serde_json::json!("retired-sequence"));
        f.sender.publish(&mut f.a, authority, &[], &Cancellation::default()).await.unwrap();
        small_asset(&mut f.a, "synthetic-retired-asset", b"synthetic retired writer asset");
        let retired = send_unconfirmed(&mut f, authority, false).await;
        let retired_job = retired.asset_job.clone().unwrap();
        let retired_files = crate::persistent_store::external_lww::publication_directory(f.directory_a.path(), &retired_job.job_id);
        let fresh = uuid::Uuid::new_v4().to_string();
        let entries = |store: &PersistentStore| {
            serde_json::to_value(store.lww_read_outbox(authority, 256).unwrap().entries).unwrap()
        };
        let original = entries(&f.a);
        let other_metadata = serde_json::to_string(&f.a.external_lww_pending(&other.target, &other_writer).unwrap().unwrap().0).unwrap();
        assert!(retired_files.exists() && other_files.exists());
        for (offered_authority, offered_writer) in [
            (DecimalU64(authority.0 + 1), old_writer.as_str()),
            (authority, other_writer.as_str()),
        ] {
            assert!(f.a.lww_adopt_fresh_writer(offered_authority, offered_writer, &fresh).is_err());
            assert!(!job_released(&f.a, &retired_job));
            assert!(!job_released(&f.a, &other_job));
            assert_eq!(entries(&f.a), original);
        }
        f.a.device_store().unwrap().connection().execute_batch(
            "CREATE TRIGGER stop_fresh_writer BEFORE UPDATE OF writer_id ON device_meta
             BEGIN SELECT RAISE(FAIL, 'synthetic-writer-adoption-stop'); END;",
        ).unwrap();
        assert!(f.a.lww_adopt_fresh_writer(authority, &old_writer, &fresh).is_err());
        assert!(job_released(&f.a, &retired_job));
        assert!(!retired_files.exists());
        assert_eq!(f.a.lww_clock_state().unwrap().writer_id, old_writer);
        assert_eq!(writer_rows(&f.a, "external_lww_segments", &old_writer), 1);
        assert_eq!(writer_rows(&f.a, "external_lww_sequences", &old_writer), 1);
        assert_eq!(entries(&f.a), original);
        f.a.device_store().unwrap().connection().execute_batch("DROP TRIGGER stop_fresh_writer").unwrap();
        f.restart_a();
        set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
        let retained = entries(&f.a);
        let before = f.a.lww_clock_state().unwrap();
        let progress = f.a.lww_binding_state().unwrap().progress;
        f.a.lww_adopt_fresh_writer(authority, &old_writer, &fresh).unwrap();
        let revision = f.a.device_store().unwrap().revision().unwrap();
        f.a.lww_adopt_fresh_writer(authority, &old_writer, &fresh).unwrap();
        assert_eq!(f.a.device_store().unwrap().revision().unwrap(), revision);
        let after = f.a.lww_clock_state().unwrap();
        assert_eq!(after.writer_id, fresh);
        assert_eq!((after.issued, after.accepted, after.binding_authority), (before.issued, before.accepted, authority));
        assert_eq!(entries(&f.a), retained);
        assert_eq!(f.a.lww_binding_state().unwrap().progress, progress);
        assert_eq!(writer_rows(&f.a, "external_lww_segments", &old_writer), 0);
        assert_eq!(writer_rows(&f.a, "external_lww_sequences", &old_writer), 0);
        assert_eq!(writer_rows(&f.a, "external_lww_segments", &other_writer), 1);
        assert_eq!(writer_rows(&f.a, "external_lww_sequences", &other_writer), 1);
        assert_eq!(serde_json::to_string(&f.a.external_lww_pending(&other.target, &other_writer).unwrap().unwrap().0).unwrap(), other_metadata);
        assert!(!job_released(&f.a, &other_job));
        assert!(other_files.exists());
    })
}
#[test]
fn removing_a_connection_drops_its_settled_and_unsent_segments_and_keeps_a_sent_one_without_files() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let target = SyncTarget::External("repository".into());
        let first = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "language"], serde_json::json!("ja"));
        f.sender.publish(&mut f.a, first, &[], &Cancellation::default()).await.unwrap();
        small_asset(&mut f.a, "synthetic-asset", b"synthetic sent asset");
        let sent = send_unconfirmed(&mut f, first, false).await;
        let sent_job = sent.asset_job.clone().expect("the sent segment holds its asset");
        let scope = f.sender.target_scope();
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        f.a.external_lww_forget_target(&scope).unwrap();
        let (kept, _) = f.a.external_lww_pending(&scope, &writer).unwrap().unwrap();
        assert_eq!(kept.seq, sent.seq);
        assert!(kept.dispatched && !kept.complete);
        assert!(kept.asset_job.is_none() && kept.assets.is_empty() && kept.reused_assets.is_empty());
        assert!(job_released(&f.a, &sent_job));
        assert_eq!(writer_rows(&f.a, "external_lww_segments", &writer), 1);
        assert_eq!(f.a.external_lww_next_sequence(&scope, &writer).unwrap(), sent.seq.0);
        let arrays: bool = f.a.device_store().unwrap().connection().query_row(
            "SELECT json_type(metadata,'$.assets')='array' AND json_type(metadata,'$.reusedAssets')='array' FROM external_lww_segments WHERE writer=?1",
            [&writer], |r| r.get(0)).unwrap();
        assert!(arrays);

        let mut g = CycleFixture::new();
        let bound = bind_external(&mut g.a, &g.sender, &target);
        small_asset(&mut g.a, "synthetic-asset", b"synthetic unsent asset");
        let unsent = seal_unsent(&mut g, bound).await;
        let unsent_job = unsent.asset_job.clone().expect("the unsent segment holds its asset");
        let writer = g.a.lww_clock_state().unwrap().writer_id;
        g.a.external_lww_forget_target(&g.sender.target_scope()).unwrap();
        assert_eq!(writer_rows(&g.a, "external_lww_segments", &writer), 0);
        assert!(job_released(&g.a, &unsent_job));
    })
}
#[test]
fn a_landed_segment_that_cleanup_removed_after_a_checkpoint_keeps_its_sequence_on_return() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let target = SyncTarget::External("repository".into());
        let first = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        let pending = send_unconfirmed(&mut f, first, true).await;
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        let job = tempfile::tempdir().unwrap();
        f.sender.compact_published(&mut f.a, job.path(), "00000000-0000-4000-8000-0000000000c1", &writer, &fake::capabilities(true), &Cancellation::default(), None).await.unwrap();
        let collection = tempfile::tempdir().unwrap();
        let (_, removed) = super::cleanup::tests::collect_after_grace(&f, collection.path()).await;
        assert!(removed.deleted_objects > 0);
        assert!(!f.provider.holds(&pending.object_id), "cleanup removes the covered segment");
        bind_elsewhere(&mut f.a);
        let rebound = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
        assert_eq!(f.sender.publish(&mut f.a, rebound, &[], &Cancellation::default()).await.unwrap().segments.0, 1);
        assert_eq!(f.provider.upload_attempts(&pending.object_id), 1);
        assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), pending.seq.0 + 2);
        f.receive_b().await;
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "ko");
        assert_eq!(f.b.read_root(None).unwrap().value["askRemoval"], true);
    })
}
#[test]
fn a_landed_segment_this_device_received_keeps_its_sequence_after_cleanup_removed_it() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let target = SyncTarget::External("repository".into());
        let first = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        let pending = send_unconfirmed(&mut f, first, true).await;
        f.receive_b().await;
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "ko");
        bind_elsewhere(&mut f.a);
        let rebound = bind_external(&mut f.a, &f.sender, &target);
        f.sender.receive_and_apply(&mut f.a, rebound, &[], &Cancellation::default()).await.unwrap();
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        assert!(f.a.lww_receive_progress(rebound).unwrap().iter().any(|p| p.writer_id.as_deref() == Some(writer.as_str()) && p.cursor == pending.seq));
        let job = tempfile::tempdir().unwrap();
        let completed = f.sender.compact_published(&mut f.a, job.path(), "00000000-0000-4000-8000-0000000000c2", &writer, &fake::capabilities(true), &Cancellation::default(), None).await.unwrap();
        let collection = tempfile::tempdir().unwrap();
        let (_, removed) = super::cleanup::tests::collect_after_grace(&f, collection.path()).await;
        assert!(removed.deleted_objects > 0);
        assert!(!f.provider.holds(&pending.object_id), "cleanup removes the covered segment");
        // The checkpoint also covers the segment; settling must answer from the
        // receive progress without reading it.
        f.provider.fail_read(&completed.reference.receipt.locator.object, ErrorKind::Transient);
        set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
        assert_eq!(f.sender.publish(&mut f.a, rebound, &[], &Cancellation::default()).await.unwrap().segments.0, 1);
        assert_eq!(f.sender.checkpoint(&completed.reference.receipt, &Cancellation::default()).await.err().unwrap().kind, ErrorKind::Transient);
        assert_eq!(f.provider.upload_attempts(&pending.object_id), 1);
        assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), pending.seq.0 + 2);
        f.receive_b().await;
        assert_eq!(f.b.read_root(None).unwrap().value["askRemoval"], true);
    })
}
#[test]
fn returning_to_a_repository_without_its_pending_state_reuses_the_sequence_of_a_segment_that_never_arrived() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let target = SyncTarget::External("repository".into());
        let first = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "language"], serde_json::json!("ja"));
        f.sender.publish(&mut f.a, first, &[], &Cancellation::default()).await.unwrap();
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        let pending = send_unconfirmed(&mut f, first, false).await;
        bind_elsewhere(&mut f.a);
        let rebound = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
        assert_eq!(f.sender.publish(&mut f.a, rebound, &[], &Cancellation::default()).await.unwrap().segments.0, 1);
        assert_eq!(f.provider.upload_attempts(&pending.object_id), 0);
        assert!(!f.provider.holds(&pending.object_id));
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), pending.seq.0 + 1);
        f.receive_b().await;
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "ja");
        assert_eq!(f.b.read_root(None).unwrap().value["askRemoval"], true);
    })
}
#[test]
fn switching_to_another_repository_never_sends_a_pending_segment_there() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        for state in ["unsent", "lost", "landed"] {
            let mut f = CycleFixture::new();
            let first = bind_external(&mut f.a, &f.sender, &SyncTarget::External("repository".into()));
            set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
            let pending = match state {
                "unsent" => seal_unsent(&mut f, first).await,
                _ => send_unconfirmed(&mut f, first, state == "landed").await,
            };
            let other = Arc::new(FakeProvider::with_object_limit(true, 128 * 1024 * 1024));
            let engine = |id: &str, root: &std::path::Path| ExternalLwwEngine {
                provider: other.clone(),
                repository: fake::repository(),
                library: "synthetic-elsewhere-library".into(),
                root_key: f.sender.root_key.clone(),
                admission: Some(Admission::synthetic(super::runtime::now_ms())),
                test_lease_clock: f.sender.test_lease_clock.clone(),
                connection_id: id.into(),
                connection_root: root.into(),
                capabilities: f.sender.capabilities.clone(),
                descriptor: f.sender.descriptor.clone(),
            };
            let mut elsewhere = engine("elsewhere", f.directory_a.path());
            let switched = bind_external(&mut f.a, &elsewhere, &SyncTarget::External("elsewhere".into()));
            set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
            assert_eq!(elsewhere.publish(&mut f.a, switched, &[], &Cancellation::default()).await.unwrap().segments.0, 1, "{state}");
            assert_eq!(other.upload_count(), 1, "{state}");
            let directory = tempfile::tempdir().unwrap();
            let mut store = PersistentStore::open(directory.path()).unwrap();
            let unset = store.read_root(None).unwrap().value["language"].clone();
            assert_ne!(unset, "ko");
            engine("reader", directory.path()).receive_and_apply(&mut store, DecimalU64(0), &[], &Cancellation::default()).await.unwrap();
            assert_eq!(store.read_root(None).unwrap().value["askRemoval"], true, "{state}");
            assert_eq!(store.read_root(None).unwrap().value["language"], unset, "{state}");
            let writer = f.a.lww_clock_state().unwrap().writer_id;
            assert_eq!(f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap().0.object_id, pending.object_id, "{state}");
        }
    })
}
#[test]
fn settling_a_segment_left_by_an_earlier_binding_releases_its_asset_pins() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        for landed in [false, true] {
            let mut f = CycleFixture::new();
            let target = SyncTarget::External("repository".into());
            let first = bind_external(&mut f.a, &f.sender, &target);
            let hash = small_asset(&mut f.a, "synthetic-detached-asset", &[41; 4096]);
            let pending = send_unconfirmed(&mut f, first, landed).await;
            assert!(pending.asset_job.is_some());
            let root = f.directory_a.path().to_owned();
            let pinned = || crate::asset_repository::job_pins::collect_durable_cas_job_roots(&root).object_hashes.contains(&hash);
            assert!(pinned(), "landed: {landed}");
            bind_elsewhere(&mut f.a);
            let rebound = bind_external(&mut f.a, &f.sender, &target);
            assert_eq!(f.sender.publish(&mut f.a, rebound, &[], &Cancellation::default()).await.unwrap().segments.0, 0);
            assert!(!pinned(), "landed: {landed}");
            let writer = f.a.lww_clock_state().unwrap().writer_id;
            assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), pending.seq.0 + u64::from(landed));
            assert!(f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().is_none());
            f.sender.receive_and_apply(&mut f.a, rebound, &[], &Cancellation::default()).await.unwrap();
        }
    })
}
#[test]
fn a_segment_left_by_an_earlier_binding_never_gives_away_a_sequence_already_seen() {
    run(async {
        use crate::persistent_store::sync_selection::SyncTarget;
        let mut f = CycleFixture::new();
        let target = SyncTarget::External("repository".into());
        let first = bind_external(&mut f.a, &f.sender, &target);
        set(&mut f.a, &["root", "language"], serde_json::json!("ko"));
        let pending = send_unconfirmed(&mut f, first, false).await;
        bind_elsewhere(&mut f.a);
        let rebound = bind_external(&mut f.a, &f.sender, &target);
        assert_eq!(f.sender.publish(&mut f.a, DecimalU64(u64::MAX), &[], &Cancellation::default()).await.err().unwrap().kind, ErrorKind::PreconditionFailed);
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        f.a.external_lww_record_seen(&f.sender.target_scope(), &writer, pending.seq.0, &"0".repeat(64), None).unwrap();
        assert_eq!(f.sender.publish(&mut f.a, rebound, &[], &Cancellation::default()).await.err().unwrap().kind, ErrorKind::Corrupt);
        assert_eq!(f.a.external_lww_pending(&f.sender.target_scope(), &writer).unwrap().unwrap().0.object_id, pending.object_id);
        assert_eq!(f.a.external_lww_next_sequence(&f.sender.target_scope(), &writer).unwrap(), pending.seq.0);
    })
}
fn third_device(f: &CycleFixture) -> (tempfile::TempDir, PersistentStore, ExternalLwwEngine) {
    let directory = tempfile::tempdir().unwrap();
    let store = PersistentStore::open(directory.path()).unwrap();
    crate::persistent_store::lww::test_clock::register(&store, f.clock.clone());
    let engine = ExternalLwwEngine {
        provider: f.provider.clone(),
        repository: fake::repository(),
        library: f.receiver.library.clone(),
        root_key: f.receiver.root_key.clone(),
        admission: Some(Admission::synthetic(super::runtime::now_ms())),
        test_lease_clock: f.receiver.test_lease_clock.clone(),
        connection_id: "third".into(),
        connection_root: directory.path().into(),
        capabilities: f.receiver.capabilities.clone(),
        descriptor: f.receiver.descriptor.clone(),
    };
    (directory, store, engine)
}
fn apply_all(store: &mut PersistentStore, requests: &[crate::persistent_store::lww::StageReceive]) {
    for request in requests {
        store.lww_stage_receive(request).unwrap();
        store
            .lww_apply_receive(&crate::persistent_store::lww::ApplyReceive {
                header: request.header.clone(),
                generating: Vec::new(),
            })
            .unwrap();
        store.lww_finish_receive(&request.header).unwrap();
    }
}
fn progress_writers(store: &PersistentStore) -> Vec<String> {
    store
        .lww_receive_progress(DecimalU64(0))
        .unwrap()
        .into_iter()
        .filter_map(|progress| progress.writer_id)
        .collect()
}
#[test]
fn behind_recovery_carries_the_published_catalog_once_across_writer_requests() {
    run(async {
        let mut f = CycleFixture::new();
        let cancel = Cancellation::default();
        set(&mut f.a, &["root", "language"], serde_json::json!("from-a"));
        f.publish_a().await;
        set(&mut f.b, &["root", "loreBookDepth"], serde_json::json!(3));
        f.receiver.publish(&mut f.b, DecimalU64(0), &[], &cancel).await.unwrap();
        let job = tempfile::tempdir().unwrap();
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        f.sender
            .compact_published(&mut f.a, job.path(), "00000000-0000-4000-8000-0000000000b1",
                &writer, &f.sender.capabilities, &cancel, None)
            .await
            .unwrap();
        for object in f.sender.listing(&cancel).await.unwrap() {
            f.provider.delete_object(&f.sender.repository, &object.locator, &cancel).await.unwrap();
        }
        let (_directory, mut store, engine) = third_device(&f);
        let requests = engine.receive_requests(&mut store, DecimalU64(0), &cancel).await.unwrap();
        assert_eq!(requests.len(), 2, "one progress request per covered writer");
        let carried = requests.iter().map(|request| request.changes.len()).sum::<usize>();
        let catalog = requests.iter().map(|request| request.changes.len()).max().unwrap();
        assert!(catalog >= 2);
        assert_eq!(carried, catalog, "the published catalog is carried by one request only");
        assert_eq!(requests[0].changes.len(), catalog, "progress never advances before the catalog applies");
        apply_all(&mut store, &requests);
        let root = store.read_root(None).unwrap().value;
        assert_eq!(root["language"], "from-a");
        assert_eq!(root["loreBookDepth"], 3);
        let mut writers = progress_writers(&store);
        writers.sort();
        let mut expected = vec![
            f.a.lww_clock_state().unwrap().writer_id,
            f.b.lww_clock_state().unwrap().writer_id,
        ];
        expected.sort();
        assert_eq!(writers, expected);
        assert!(engine.receive_requests(&mut store, DecimalU64(0), &cancel).await.unwrap().is_empty());
    })
}
#[test]
fn receive_pages_keep_change_order_within_the_budget_and_send_a_larger_change_alone() {
    use crate::persistent_store::lww::Change;
    let change = |name: &str, length: usize| Change {
        key: UnitKey::new(&["root", name]).unwrap(),
        stamp: risunest_sync_wire::stamp::Stamp { physical_ms: DecimalU64(1), logical: 0, writer_id: "synthetic-writer".into() },
        value: risunest_sync_wire::unit::UnitValue::Inline { bytes: "A".repeat(length) },
    };
    let (first, second) = (change("language", 10), change("askRemoval", 10));
    let budget = [&first, &second].iter().map(|change| serde_json::to_vec(change).unwrap().len() + 1).sum::<usize>();
    let changes = vec![first, second, change("loreBookDepth", budget * 2), change("additionalPrompt", 10)];
    let pages = super::lww_engine::receive_pages(changes.clone(), budget).unwrap();
    assert_eq!(pages.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1, 1]);
    assert_eq!(pages.concat(), changes);
    assert_eq!(super::lww_engine::receive_pages(Vec::new(), budget).unwrap(), [Vec::<Change>::new()]);
}
#[test]
fn a_segment_over_the_page_budget_arrives_in_pages_and_resumes_after_the_finished_ones() {
    run(async {
        let mut f = CycleFixture::new();
        let cancel = Cancellation::default();
        set(&mut f.a, &["root", "language"], serde_json::json!("paged"));
        set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
        set(&mut f.a, &["root", "loreBookDepth"], serde_json::json!(7));
        set(&mut f.a, &["root", "additionalPrompt"], serde_json::json!("synthetic prompt"));
        assert_eq!(f.publish_a().await.segments.0, 1);
        super::lww_engine::set_receive_page_bytes_for_test(1);
        let requests = f.receiver.receive_requests(&mut f.b, DecimalU64(0), &cancel).await.unwrap();
        assert!(requests.len() >= 4);
        assert!(requests.iter().all(|request| request.changes.len() == 1));
        let last = requests.last().unwrap().progress.clone();
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        assert_eq!(last.writer_id.as_deref(), Some(writer.as_str()));
        assert!(requests[..requests.len() - 1].iter().all(|request| request.progress.cursor.0 == last.cursor.0 - 1));
        apply_all(&mut f.b, &requests[..2]);
        assert!(f.b.lww_receive_progress(DecimalU64(0)).unwrap().is_empty(), "progress waits for the last page");
        let resumed = f.receiver.receive_requests(&mut f.b, DecimalU64(0), &cancel).await.unwrap();
        assert_eq!(serde_json::to_value(&resumed).unwrap(), serde_json::to_value(&requests[2..]).unwrap());
        apply_all(&mut f.b, &resumed);
        let root = f.b.read_root(None).unwrap().value;
        assert_eq!(root["language"], "paged");
        assert_eq!(root["askRemoval"], true);
        assert_eq!(root["loreBookDepth"], 7);
        assert_eq!(root["additionalPrompt"], "synthetic prompt");
        let progress = f.b.lww_receive_progress(DecimalU64(0)).unwrap();
        assert_eq!(progress.len(), 1);
        assert_eq!(progress[0].cursor, last.cursor);
        assert!(f.receiver.receive_requests(&mut f.b, DecimalU64(0), &cancel).await.unwrap().is_empty());
    })
}
#[test]
fn behind_recovery_pages_the_catalog_and_resumes_after_the_finished_pages() {
    run(async {
        let mut f = CycleFixture::new();
        let cancel = Cancellation::default();
        set(&mut f.a, &["root", "language"], serde_json::json!("from-a"));
        set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
        f.publish_a().await;
        set(&mut f.b, &["root", "loreBookDepth"], serde_json::json!(3));
        f.receiver.publish(&mut f.b, DecimalU64(0), &[], &cancel).await.unwrap();
        let job = tempfile::tempdir().unwrap();
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        f.sender
            .compact_published(&mut f.a, job.path(), "00000000-0000-4000-8000-0000000000b2",
                &writer, &f.sender.capabilities, &cancel, None)
            .await
            .unwrap();
        for object in f.sender.listing(&cancel).await.unwrap() {
            f.provider.delete_object(&f.sender.repository, &object.locator, &cancel).await.unwrap();
        }
        let (_directory, mut store, engine) = third_device(&f);
        super::lww_engine::set_receive_page_bytes_for_test(1);
        let requests = engine.receive_requests(&mut store, DecimalU64(0), &cancel).await.unwrap();
        let catalog = requests.iter().take_while(|request| !request.changes.is_empty()).count();
        assert!(catalog >= 3);
        assert_eq!(requests.len(), catalog + 1, "the other writer's progress follows the catalog pages");
        assert!(requests[..catalog].iter().all(|request| request.changes.len() == 1));
        assert!(requests[catalog].changes.is_empty());
        let carrier = requests[0].progress.writer_id.clone();
        assert!(requests[..catalog].iter().all(|request| request.progress.writer_id == carrier));
        assert!(requests[..catalog - 1].iter().all(|request| request.progress.cursor == DecimalU64(0)));
        assert_ne!(requests[catalog - 1].progress.cursor, DecimalU64(0));
        apply_all(&mut store, &requests[..catalog - 1]);
        assert!(progress_writers(&store).is_empty(), "progress waits for the last catalog page");
        let resumed = engine.receive_requests(&mut store, DecimalU64(0), &cancel).await.unwrap();
        assert_eq!(serde_json::to_value(&resumed).unwrap(), serde_json::to_value(&requests[catalog - 1..]).unwrap());
        apply_all(&mut store, &resumed);
        let root = store.read_root(None).unwrap().value;
        assert_eq!(root["language"], "from-a");
        assert_eq!(root["askRemoval"], true);
        assert_eq!(root["loreBookDepth"], 3);
        let mut writers = progress_writers(&store);
        writers.sort();
        let mut expected = vec![f.a.lww_clock_state().unwrap().writer_id, f.b.lww_clock_state().unwrap().writer_id];
        expected.sort();
        assert_eq!(writers, expected);
        assert!(engine.receive_requests(&mut store, DecimalU64(0), &cancel).await.unwrap().is_empty());
    })
}
#[test]
fn a_future_stamped_writer_is_held_while_other_writers_are_received() {
    run(async {
        let mut f = CycleFixture::new();
        let cancel = Cancellation::default();
        set(&mut f.a, &["root", "language"], serde_json::json!("future"));
        f.publish_a().await;
        let id = f.provider.uploaded_ids()[0].clone();
        let bytes = f.provider.contents(&id).unwrap();
        f.provider.forget(&id);
        let (writer, seq, _) = parse_segment_object_id(&id).unwrap();
        let mut payload = lww_segment::open(&bytes, &f.sender.library, writer, seq, &[7; 32]).unwrap();
        payload.changes[0].stamp.physical_ms = DecimalU64(super::runtime::now_ms() + 600_000);
        let (sealed, _) = lww_segment::seal(&payload, &[7; 32]).unwrap();
        let future = segment_object_id(writer, seq, &lww_segment::digest(&sealed)).unwrap();
        f.provider.seed(&future, ObjectRole::Segment, sealed);
        set(&mut f.b, &["root", "loreBookDepth"], serde_json::json!(5));
        f.receiver.publish(&mut f.b, DecimalU64(0), &[], &cancel).await.unwrap();
        let (_directory, mut store, engine) = third_device(&f);
        assert_eq!(engine.receive_and_apply(&mut store, DecimalU64(0), &[], &cancel).await.unwrap(), 1);
        assert_eq!(store.read_root(None).unwrap().value["loreBookDepth"], 5);
        assert_eq!(progress_writers(&store), [f.b.lww_clock_state().unwrap().writer_id]);
        assert_eq!(
            engine.receive_requests(&mut store, DecimalU64(0), &cancel).await.err().unwrap().kind,
            ErrorKind::ClockSkew,
            "a held writer still reports skew when nothing else arrives"
        );
        assert_eq!(progress_writers(&store), [f.b.lww_clock_state().unwrap().writer_id]);
    })
}
#[test]
fn segment_assembly_hashes_each_page_a_bounded_number_of_times() {
    run(async {
        let mut f = CycleFixture::new();
        let count = 300;
        let mut units = vec![UnitMutation::Set {
            key: UnitKey::new(&["exists", "character", "char"]).unwrap(),
            value: serde_json::json!({"type":"character"}),
        }];
        for index in 0..count {
            units.push(UnitMutation::Set {
                key: UnitKey::new(&["exists", "conversation", "char", &format!("conv-{index}")]).unwrap(),
                value: serde_json::json!(true),
            });
        }
        f.a.commit(&WorkingSetCommit { expected_revision: f.a.revision().unwrap(), unit_mutations: Some(units), ..Default::default() }).unwrap();
        let conversations = (0..count).map(|index| ConversationMutation::ReplaceRange {
            character_id: "char".into(), conversation_id: format!("conv-{index}"), start: 0, delete_count: 0,
            messages: vec![serde_json::json!({"chatId":format!("synthetic-{index}"),"data":"x".repeat(4096)})],
            conversation: None, configured_index: None,
        }).collect();
        f.a.commit(&WorkingSetCommit { expected_revision: f.a.revision().unwrap(), conversations: Some(conversations), ..Default::default() }).unwrap();
        lww_segment::reset_hash_bytes();
        assert_eq!(f.publish_a().await.segments.0, 1);
        let hashed = lww_segment::take_hash_bytes();
        let object = f.sender.listing(&Cancellation::default()).await.unwrap().remove(0);
        let bytes = f.provider.contents(&object.locator.object).unwrap();
        let (writer, seq, _) = parse_segment_object_id(&object.locator.object).unwrap();
        let payload = lww_segment::open(&bytes, &f.sender.library, writer, seq, &f.sender.root_key).unwrap();
        assert!(payload.message_pages.len() >= count);
        assert!(hashed <= 24 * bytes.len() as u64, "hashed {hashed} bytes to publish a {} byte segment", bytes.len());
    })
}
#[test]
fn captured_large_bodies_reserve_the_longest_locator_sealing_can_add() {
    let repository = fake::repository();
    let mut placeholder = lww_segment::Segment::new("synthetic-lww-library", "00000000-0000-4000-8000-000000000001", 1);
    for index in 0..3u8 {
        placeholder.large_bodies.insert(risunest_sync_wire::hash(&[index]), lww_segment::LargeBody {
            object_id: format!("00000000-0000-4000-8000-00000000010{index}"),
            sha256: "0".repeat(64),
            byte_length: DecimalU64(0),
            plaintext_byte_length: DecimalU64(lww_segment::SMALL_BODY_BYTES as u64 + 1),
            locator: None,
        });
    }
    let mut filled = placeholder.clone();
    for body in filled.large_bodies.values_mut() {
        body.sha256 = "f".repeat(64);
        body.byte_length = DecimalU64(u64::MAX);
        let locator = RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: Some("\u{1}".repeat(super::lww_engine::MAX_LOCATOR_COLLECTION_BYTES)),
            object: "\u{1}".repeat(8192),
        };
        locator.validate_for(&repository).unwrap();
        body.locator = Some(locator);
    }
    let admitted = ExternalLwwEngine::capture_length(
        &placeholder, &Default::default(), &Default::default(), &repository,
    ).unwrap();
    assert!(filled.encode().unwrap().len() <= admitted, "{} > {admitted}", filled.encode().unwrap().len());
}
#[test]
fn segment_assembly_writes_control_pages_to_disk_instead_of_holding_them() {
    run(async {
        let mut f = CycleFixture::new();
        conversation(&mut f.a);
        let messages = (0..40).map(|index| serde_json::json!({
            "chatId": format!("synthetic-spill-{index}"), "data": "x".repeat(225 * 1024),
        })).collect::<Vec<_>>();
        f.a.commit(&WorkingSetCommit { expected_revision: f.a.revision().unwrap(),
            conversations: Some(vec![ConversationMutation::ReplaceRange { character_id: "char".into(), conversation_id: "conv".into(),
                start: 0, delete_count: 0, messages, conversation: None, configured_index: None }]), ..Default::default() }).unwrap();
        super::lww_engine::cycle_keys::reset();
        assert_eq!(f.publish_a().await.segments.0, 1);
        let held = super::lww_engine::cycle_keys::take().buffered_control_bytes;
        assert!(held < lww_segment::SMALL_BODY_BYTES, "held {held} control bytes in memory while assembling a segment");
        assert_eq!(f.receive_b().await, 1);
    })
}
pub(crate) fn device_rows(store: &PersistentStore, table: &str) -> i64 {
    store.device_store().unwrap().connection()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
}
/// Stages the published state on a fresh device and activates it as the
/// binding flow does: stage, switch, then replace from the stage.
pub(crate) async fn bootstrapped_device(f: &CycleFixture) -> (tempfile::TempDir, PersistentStore, ExternalLwwEngine, DecimalU64) {
    use crate::persistent_store::sync_selection::{ReplaceBindingRequest, SwitchBindingRequest, SyncTarget};
    let (directory, mut store, engine) = third_device(f);
    let (header, inspection) = binding_context(&store, &engine, &uuid::Uuid::new_v4().to_string());
    let stage = engine.stage_binding(&mut store, &header, &inspection, &Cancellation::default()).await.unwrap();
    let state = store.lww_binding_state().unwrap();
    let state = store.switch_lww_binding(&SwitchBindingRequest {
        initial_publication: false,
        header: crate::persistent_store::lww::Header { binding_authority: state.target_authority, request_id: uuid::Uuid::new_v4().to_string() },
        expected_selection_epoch: state.selection_epoch, target: SyncTarget::External(engine.connection_id.clone()), inspection_id: Some(inspection),
    }).unwrap();
    store.replace_lww_binding(&ReplaceBindingRequest {
        header: crate::persistent_store::lww::Header { binding_authority: state.target_authority, request_id: header.request_id.clone() },
        expected_selection_epoch: state.selection_epoch, staging_id: stage.staging_id, receive_id: header.request_id,
        target_id: engine.repository.connection_identity.clone(), library_id: engine.library.clone(),
    }).unwrap();
    (directory, store, engine, state.target_authority)
}
#[test]
fn a_device_bootstrapped_from_a_checkpoint_never_reads_the_covered_segments_still_listed() {
    run(async {
        let mut f = CycleFixture::new();
        let cancel = Cancellation::default();
        for value in ["one", "two", "three"] {
            set(&mut f.a, &["root", "language"], serde_json::json!(value));
            f.publish_a().await;
        }
        let job = tempfile::tempdir().unwrap();
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        f.sender.compact_published(&mut f.a, job.path(), "00000000-0000-4000-8000-0000000000d1",
            &writer, &f.sender.capabilities, &cancel, None).await.unwrap();
        let segments = f.sender.listing(&cancel).await.unwrap().into_iter().map(|receipt| receipt.locator.object).collect::<Vec<_>>();
        assert_eq!(segments.len(), 3);
        let (_directory, mut store, engine, authority) = bootstrapped_device(&f).await;
        assert_eq!(store.read_root(None).unwrap().value["language"], "three");
        let reads = |f: &CycleFixture| segments.iter().map(|id| f.provider.read_attempts(id)).collect::<Vec<_>>();
        let before = reads(&f);
        for _ in 0..2 {
            let requests = engine.receive_requests(&mut store, authority, &cancel).await.unwrap();
            assert!(requests.is_empty(), "covered history is received again: {:?}",
                requests.iter().map(|request| &request.header.request_id).collect::<Vec<_>>());
        }
        assert_eq!(reads(&f), before, "a covered segment is downloaded again after the bootstrap");
        set(&mut f.a, &["root", "language"], serde_json::json!("four"));
        f.publish_a().await;
        let requests = engine.receive_requests(&mut store, authority, &cancel).await.unwrap();
        assert_eq!(requests.len(), 1);
        apply_all(&mut store, &requests);
        assert_eq!(store.read_root(None).unwrap().value["language"], "four");
        assert_eq!(reads(&f), before);
    })
}
#[test]
fn finished_publications_and_received_pages_leave_no_rows_behind() {
    run(async {
        let mut f = CycleFixture::new();
        let cancel = Cancellation::default();
        for (index, value) in ["one", "two", "three"].into_iter().enumerate() {
            set(&mut f.a, &["root", "language"], serde_json::json!(value));
            if index == 1 { small_asset(&mut f.a, "retained-large", &vec![29; 5 * 1024 * 1024]); }
            assert_eq!(f.publish_a().await.segments.0, 1);
            assert_eq!(device_rows(&f.a, "external_lww_segments"), 0, "a finished publication keeps its row");
            assert_eq!(f.receive_b().await, 1);
            assert_eq!(f.receive_b().await, 0);
            assert_eq!(device_rows(&f.b, "external_lww_receives"), 0, "a finished page keeps its row");
        }
        assert_eq!(f.b.read_root(None).unwrap().value["language"], "three");
        set(&mut f.b, &["root", "loreBookDepth"], serde_json::json!(3));
        f.receiver.publish(&mut f.b, DecimalU64(0), &[], &cancel).await.unwrap();
        let job = tempfile::tempdir().unwrap();
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        f.sender.compact_published(&mut f.a, job.path(), "00000000-0000-4000-8000-0000000000d2",
            &writer, &f.sender.capabilities, &cancel, None).await.unwrap();
        for object in f.sender.listing(&cancel).await.unwrap() {
            f.provider.delete_object(&f.sender.repository, &object.locator, &cancel).await.unwrap();
        }
        let (_directory, mut store, engine) = third_device(&f);
        super::lww_engine::set_receive_page_bytes_for_test(1);
        let requests = engine.receive_requests(&mut store, DecimalU64(0), &cancel).await.unwrap();
        assert!(requests.len() >= 3);
        apply_all(&mut store, &requests);
        assert!(engine.receive_requests(&mut store, DecimalU64(0), &cancel).await.unwrap().is_empty());
        assert_eq!(device_rows(&store, "external_lww_receives"), 0, "the catalog pages of a behind recovery stay");
    })
}
#[test]
fn each_received_segment_keeps_its_pages_before_the_next_segment_is_read() {
    run(async {
        let mut f = CycleFixture::new();
        let cancel = Cancellation::default();
        set(&mut f.a, &["root", "language"], serde_json::json!("one"));
        f.publish_a().await;
        set(&mut f.a, &["root", "askRemoval"], serde_json::json!(true));
        f.publish_a().await;
        let second = f.sender.listing(&cancel).await.unwrap().into_iter().map(|receipt| receipt.locator.object)
            .find(|id| parse_segment_object_id(id).unwrap().1 == 2).unwrap();
        f.provider.fail_read(&second, ErrorKind::Transient);
        assert_eq!(f.receiver.receive_requests(&mut f.b, DecimalU64(0), &cancel).await.err().unwrap().kind, ErrorKind::Transient);
        assert_eq!(device_rows(&f.b, "external_lww_receives"), 1, "the first segment's page waits until every segment is read");
        assert_eq!(f.receive_b().await, 2);
        assert_eq!(f.b.read_root(None).unwrap().value["askRemoval"], true);
    })
}
#[test]
fn a_large_body_waits_for_its_upload_in_a_file_rather_than_in_the_publication_row() {
    run(async {
        let mut f = CycleFixture::new();
        let body = vec![41; 5 * 1024 * 1024];
        let hash = small_asset(&mut f.a, "large", &body);
        let pending = seal_unsent(&mut f, DecimalU64(0)).await;
        assert_eq!(pending.bodies.len(), 1);
        let metadata: i64 = f.a.device_store().unwrap().connection()
            .query_row("SELECT max(length(metadata)) FROM external_lww_segments", [], |r| r.get(0)).unwrap();
        assert!(metadata < 64 * 1024, "the publication row holds {metadata} bytes");
        let job = pending.asset_job.as_ref().unwrap().job_id.clone();
        let publication = f.directory_a.path().join("external-storage").join("lww-publications").join(&job);
        let sealed = publication.join("bodies").join(&pending.bodies[0].object_id);
        assert_eq!(std::fs::metadata(&sealed).unwrap().len(), pending.bodies[0].byte_length);
        assert_eq!(f.publish_a().await.segments.0, 1);
        assert!(!publication.exists(), "a finished publication leaves its files");
        assert_eq!(f.provider.upload_attempts(&pending.bodies[0].object_id), 1);
        assert_eq!(f.receive_b().await, 1);
        let mut file = hydrate_receiver(&f, &hash).await;
        let mut restored = Vec::new();
        std::io::Read::read_to_end(&mut file, &mut restored).unwrap();
        assert!(restored == body);
    })
}
#[test]
fn a_second_compaction_reads_only_the_segments_above_the_retained_checkpoint() {
    run(async {
        let mut f = CycleFixture::new();
        let cancel = Cancellation::default();
        let writer = f.a.lww_clock_state().unwrap().writer_id;
        for value in ["one", "two", "three"] {
            set(&mut f.a, &["root", "language"], serde_json::json!(value));
            f.publish_a().await;
        }
        let job = tempfile::tempdir().unwrap();
        f.sender.compact_published(&mut f.a, &job.path().join("first"), "00000000-0000-4000-8000-0000000000d3",
            &writer, &f.sender.capabilities, &cancel, None).await.unwrap();
        let covered = f.sender.listing(&cancel).await.unwrap().into_iter().map(|receipt| receipt.locator.object).collect::<Vec<_>>();
        let reads = |f: &CycleFixture| covered.iter().map(|id| f.provider.read_attempts(id)).collect::<Vec<_>>();
        let before = reads(&f);
        for value in ["four", "five"] {
            set(&mut f.a, &["root", "language"], serde_json::json!(value));
            f.publish_a().await;
        }
        let completed = f.sender.compact_published(&mut f.a, &job.path().join("second"), "00000000-0000-4000-8000-0000000000d4",
            &writer, &f.sender.capabilities, &cancel, None).await.unwrap();
        assert_eq!(reads(&f), before, "a compaction reads segments its retained checkpoint covers");
        let (_, checkpoint) = f.sender.checkpoint(&completed.reference.receipt, &cancel).await.unwrap();
        assert_eq!(checkpoint.covered_prefixes.get(&writer), Some(&DecimalU64(5)));
        let (_directory, store, _engine, _) = bootstrapped_device(&f).await;
        assert_eq!(store.read_root(None).unwrap().value["language"], "five");
    })
}
#[test]
fn one_listing_call_answers_hundreds_of_segments() {
    run(async {
        let f = CycleFixture::new();
        let writer = uuid::Uuid::new_v4().to_string();
        for seq in 1..=250u64 {
            let bytes = seq.to_le_bytes().to_vec();
            let id = segment_object_id(&writer, seq, &lww_segment::digest(&bytes)).unwrap();
            f.provider.seed(&id, ObjectRole::Segment, bytes);
        }
        let before = f.provider.listing_count();
        assert_eq!(f.sender.listing(&Cancellation::default()).await.unwrap().len(), 250);
        assert_eq!(f.provider.listing_count() - before, 1);
        for index in 0..150 {
            f.provider.seed(&format!("synthetic-checkpoint-{index:03}"), ObjectRole::Snapshot, vec![1]);
        }
        let before = f.provider.listing_count();
        assert_eq!(f.sender.checkpoint_receipts(&Cancellation::default()).await.unwrap().len(), 150);
        assert_eq!(f.provider.listing_count() - before, 1);
    })
}
#[test]
fn incomparable_checkpoints_compact_into_one_that_covers_both_in_either_listing_order() {
    run(async {
        let mut f = CycleFixture::new();
        let cancel = Cancellation::default();
        let a = f.a.lww_clock_state().unwrap().writer_id;
        let b = f.b.lww_clock_state().unwrap().writer_id;
        for (depth, value) in [(1, "a-one"), (2, "a-two")] {
            set(&mut f.a, &["root", "language"], serde_json::json!(value));
            f.publish_a().await;
            set(&mut f.b, &["root", "loreBookDepth"], serde_json::json!(depth));
            f.receiver.publish(&mut f.b, DecimalU64(0), &[], &cancel).await.unwrap();
        }
        let listed = f.sender.listing(&cancel).await.unwrap().into_iter().map(|receipt| receipt.locator.object).collect::<Vec<_>>();
        let segment = |writer: &str, seq: u64| listed.iter().find(|id| {
            let (owner, number, _) = parse_segment_object_id(id).unwrap();
            owner == writer && number == seq
        }).unwrap().clone();
        let (a2, b2) = (segment(&a, 2), segment(&b, 2));
        let hide = |f: &CycleFixture, ids: &[&String]| ids.iter().map(|id| {
            let bytes = f.provider.contents(id).unwrap();
            f.provider.forget(id);
            ((*id).clone(), bytes)
        }).collect::<Vec<_>>();
        let restore = |f: &CycleFixture, hidden: Vec<(String, Vec<u8>)>, role: ObjectRole| {
            for (id, bytes) in hidden { f.provider.seed(&id, role, bytes); }
        };
        let job = tempfile::tempdir().unwrap();
        let mut identities = Vec::new();
        for (first, second, merged) in [("f1", "f2", "f3"), ("f6", "f5", "f7")] {
            let id = |suffix: &str| format!("00000000-0000-4000-8000-0000000000{suffix}");
            let hidden = hide(&f, &[&b2]);
            let x = f.sender.compact_published(&mut f.a, &job.path().join(first), &id(first), &a, &f.sender.capabilities, &cancel, None).await.unwrap();
            restore(&f, hidden, ObjectRole::Segment);
            let x_object = x.reference.receipt.locator.object.clone();
            let hidden = hide(&f, &[&a2]);
            let hidden_x = hide(&f, &[&x_object]);
            let y = f.sender.compact_published(&mut f.a, &job.path().join(second), &id(second), &a, &f.sender.capabilities, &cancel, None).await.unwrap();
            restore(&f, hidden, ObjectRole::Segment);
            restore(&f, hidden_x, ObjectRole::Snapshot);
            let y_object = y.reference.receipt.locator.object.clone();
            let (_, x) = f.sender.checkpoint(&x.reference.receipt, &cancel).await.unwrap();
            let (_, y) = f.sender.checkpoint(&y.reference.receipt, &cancel).await.unwrap();
            assert_eq!(x.covered_prefixes, std::collections::BTreeMap::from([(a.clone(), DecimalU64(2)), (b.clone(), DecimalU64(1))]));
            assert_eq!(y.covered_prefixes, std::collections::BTreeMap::from([(a.clone(), DecimalU64(1)), (b.clone(), DecimalU64(2))]));
            let both = f.sender.checkpoints(&cancel).await.unwrap().into_iter().map(|(_, checkpoint)| checkpoint).collect::<Vec<_>>();
            assert_eq!(both.len(), 2);
            assert_eq!(super::lww_checkpoint::retained(&both).unwrap().len(), 2, "neither checkpoint covers the other");
            let completed = f.sender.compact_published(&mut f.a, &job.path().join(merged), &id(merged), &a, &f.sender.capabilities, &cancel, None).await.unwrap();
            let (object, checkpoint) = f.sender.checkpoint(&completed.reference.receipt, &cancel).await.unwrap();
            assert_eq!(checkpoint.covered_prefixes, std::collections::BTreeMap::from([(a.clone(), DecimalU64(2)), (b.clone(), DecimalU64(2))]));
            let all = f.sender.checkpoints(&cancel).await.unwrap().into_iter().map(|(_, checkpoint)| checkpoint).collect::<Vec<_>>();
            let retained = super::lww_checkpoint::retained(&all).unwrap();
            assert!(retained.len() == 1 && retained.contains(&checkpoint.snapshot_id), "the merged checkpoint alone is retained");
            identities.push(checkpoint.state_identity.clone());
            hide(&f, &[&object.receipt.locator.object, &x_object, &y_object]);
        }
        assert_eq!(identities[0], identities[1], "the merged state does not depend on the listing order");
    })
}
#[test]
fn a_compactor_without_plugin_local_participation_keeps_published_plugin_local_units() {
    run(async {
        use crate::persistent_store::device_store::{plugin_values::PluginDeviceMutation, Section};
        let mut f = CycleFixture::new();
        let cancel = Cancellation::default();
        f.a.device_store_mut().unwrap().set_section_participating(Section::LocalPlugins, true).unwrap();
        f.a.device_store_mut().unwrap().write_plugin_device_values("synthetic-owner", &[PluginDeviceMutation::Set {
            space: "string".into(), key: "synthetic-key".into(), value: "synthetic plugin value".into(),
        }]).unwrap();
        assert_eq!(f.publish_a().await.segments.0, 1);
        let key = UnitKey::new(&["plugin-local", "synthetic-owner", "string", "synthetic-key"]).unwrap();
        let job = tempfile::tempdir().unwrap();
        let writer = f.b.lww_clock_state().unwrap().writer_id;
        let completed = f.receiver.compact_published(&mut f.b, job.path(), "00000000-0000-4000-8000-0000000000e5",
            &writer, &f.receiver.capabilities, &cancel, None).await.unwrap();
        let (_, checkpoint) = f.receiver.checkpoint(&completed.reference.receipt, &cancel).await.unwrap();
        assert_eq!(checkpoint.covered_prefixes.get(&f.a.lww_clock_state().unwrap().writer_id), Some(&DecimalU64(1)));
        let inputs = tempfile::tempdir().unwrap();
        let state = f.receiver.published_state(&mut f.b, inputs.path(), &cancel).await.unwrap();
        assert!(state.segments.is_empty(), "the checkpoint covers every segment");
        assert!(state.catalog.changes().unwrap().iter().any(|change| change.key.as_str() == key.as_str()),
            "a compactor that does not take part in plugin-local sync drops a published plugin-local unit");
    })
}
