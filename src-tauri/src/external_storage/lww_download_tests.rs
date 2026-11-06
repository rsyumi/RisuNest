use super::*;
use crate::external_storage::{fake,lww_tests::CycleFixture,snapshot_restore::download_tests::DownloadProvider};

fn sources(f:&CycleFixture,provider:&DownloadProvider,count:usize)->Vec<Source> {
    (0..count).map(|index| {
        let bytes=format!("synthetic standalone hydration {index}").into_bytes();let id=format!("body-{index}");
        let mut sealed=Vec::new();segment::seal_body_stream(&mut std::io::Cursor::new(&bytes),&mut sealed,&f.receiver.library,&id,&[7;32],bytes.len() as u64).unwrap();
        let source=Source{hash:risunest_sync_wire::hash(&bytes),library_id:f.receiver.library.clone(),connection_id:"receiver".into(),
            connection_root:f.directory_b.path().into(),protected_segment:"synthetic-segment".into(),
            body:LargeBody{object_id:id.clone(),sha256:risunest_sync_wire::hash(&sealed),byte_length:(sealed.len() as u64).into(),
                plaintext_byte_length:(bytes.len() as u64).into(),locator:Some(RemoteLocator{connection_identity:f.receiver.repository.connection_identity.clone(),collection:None,object:id.clone()})}};
        provider.inner.seed(&id,ObjectRole::Segment,sealed);register(f.directory_b.path(),&source).unwrap();source
    }).collect()
}
fn connection(f:&CycleFixture,provider:Arc<dyn Provider>,limit:Option<usize>)->super::super::connection_commands::ConnectedRepository {
    let stored=StoredConnection{id:"receiver".into(),config:ConnectionConfig{provider:"synthetic".into(),profile:None,endpoint:"https://synthetic.invalid".into(),account_id:"fixture".into(),location:Default::default(),oauth_profile:None},
        descriptor:f.receiver.descriptor.clone(),descriptor_locator:RemoteLocator{connection_identity:f.receiver.repository.connection_identity.clone(),collection:None,object:"descriptor".into()},
        provider_repository_id:f.receiver.repository.repository_id.clone(),credential_ref:"credential".into(),root_key_ref:"key".into(),recovery_key_ref:"recovery".into(),retention_policy:None,
        transfer_concurrency:limit,labels:Default::default(),capabilities:f.receiver.capabilities.clone(),created_at_ms:1,verified_at_ms:1,last_sync_at_ms:None,last_backup_at_ms:None};
    super::super::connection_commands::ConnectedRepository{stored,provider,handle:fake::repository(),dependencies:fake::loopback_dependencies(fake::MemoryVault::default(),1).dependencies,root_key:zeroize::Zeroizing::new([7;32])}
}
fn runtime()->tokio::runtime::Runtime {tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()}

#[test]
fn standalone_hydration_transfers_four_bodies_and_publishes_each_identity_once() {
    runtime().block_on(async {
        let f=CycleFixture::new();let provider=Arc::new(DownloadProvider::new(4,&["body-0","body-1","body-2","body-3"]));
        let sources=sources(&f,&provider,6);let connection=Arc::new(connection(&f,provider.clone(),None));
        let _installed=install_test_source_connection(f.directory_b.path(),connection).unwrap();
        let root=f.directory_b.path().to_owned();let hashes:Vec<_>=sources.iter().map(|s|s.hash.clone()).collect();let copied=hashes.clone();
        let task=tokio::task::spawn_blocking(move || {
            let mut completed=Vec::new();let unavailable=hydrate_registered_many(&root,&copied,&BTreeSet::new(),None,&||Ok(()),&mut |hash,_|completed.push(hash.to_owned())).unwrap();
            assert!(unavailable.is_empty());completed
        });
        tokio::time::timeout(fake::DEADLOCK_TEST_BOUND,async {
            provider.started(4).await;assert_eq!(provider.peak(),4);
            for id in ["body-3","body-2","body-1","body-0"] {provider.release(id);}
            let completed=task.await.unwrap();assert_eq!(completed.len(),6);assert_eq!(completed.into_iter().collect::<BTreeSet<_>>(),hashes.into_iter().collect());
        }).await.expect("standalone hydration deadlocked");
        let cas=crate::asset_repository::PayloadCas::new(f.directory_b.path()).unwrap();
        for (index,source) in sources.iter().enumerate() {assert_eq!(cas.read_object(&source.hash).unwrap().unwrap(),format!("synthetic standalone hydration {index}").into_bytes());}
    });
}

#[test]
fn wrapped_remote_source_spooling_and_hydration_finish_with_one_body_slot() {
    runtime().block_on(async {
        let f=CycleFixture::new();let provider=Arc::new(DownloadProvider::new(4,&[]));let sources=sources(&f,&provider,2);
        let mut connection=connection(&f,provider.clone(),Some(1));
        ConnectionStore::open(f.directory_b.path()).unwrap().insert(&connection.stored).unwrap();
        connection.provider=super::super::transfer_limit::wrap(f.directory_b.path(),"receiver",connection.provider).unwrap();
        let _installed=install_test_source_connection(f.directory_b.path(),Arc::new(connection)).unwrap();
        let scratch=tempfile::tempdir().unwrap();
        let downloaded=Arc::new(std::sync::atomic::AtomicU64::new(0)); let observed=downloaded.clone();
        let observer=Arc::new(move |bytes|{observed.fetch_add(bytes,std::sync::atomic::Ordering::Relaxed);});
        let barrier=provider.inner.arm_partial_read_barrier("body-0",16).unwrap();
        let source=FrozenBodySource::Standalone(sources[0].clone());let destination=scratch.path().to_owned();
        let task=tokio::spawn(async move {spool_frozen_remote_body_observed(&source,&destination,&Cancellation::default(),Some(observer)).await});
        tokio::time::timeout(std::time::Duration::from_secs(20),barrier.reached.notified()).await.expect("standalone first chunk did not arrive");
        assert_eq!(downloaded.load(std::sync::atomic::Ordering::Relaxed),16,"a held response must expose its first chunk before completion");
        assert!(!task.is_finished());
        barrier.release();
        let spool=tokio::time::timeout(std::time::Duration::from_secs(20),task).await.expect("one-slot source spool deadlocked").unwrap().unwrap();
        assert_eq!(downloaded.load(std::sync::atomic::Ordering::Relaxed),sources[0].body.byte_length.0);
        assert_eq!(std::fs::read(spool.path()).unwrap(),b"synthetic standalone hydration 0");
        assert_eq!(downloaded.load(std::sync::atomic::Ordering::Relaxed),sources[0].body.byte_length.0,"reading plaintext must not count as another download");
        let root=f.directory_b.path().to_owned();let hashes=sources.iter().map(|s|s.hash.clone()).collect::<Vec<_>>();
        tokio::time::timeout(fake::DEADLOCK_TEST_BOUND,tokio::task::spawn_blocking(move ||hydrate_registered_many(&root,&hashes,&BTreeSet::new(),None,&||Ok(()),&mut |_,_|{}))).await.expect("one-slot hydration deadlocked").unwrap().unwrap();
        assert_eq!(provider.peak(),1);
    });
}

#[test]
fn standalone_hydration_settles_successful_siblings_before_returning_a_download_error() {
    runtime().block_on(async {
        let f=CycleFixture::new();let provider=Arc::new(DownloadProvider::new(4,&["body-0","body-1","body-2","body-3"]));
        let sources=sources(&f,&provider,6);provider.inner.fail_read("body-1",ErrorKind::Transient);
        let _installed=install_test_source_connection(f.directory_b.path(),Arc::new(connection(&f,provider.clone(),None))).unwrap();
        let root=f.directory_b.path().to_owned();let hashes=sources.iter().map(|s|s.hash.clone()).collect::<Vec<_>>();
        let task=tokio::task::spawn_blocking(move || {
            let mut completed=Vec::new();let result=hydrate_registered_many(&root,&hashes,&BTreeSet::new(),None,&||Ok(()),&mut |hash,_|completed.push(hash.to_owned()));
            (result,completed)
        });
        tokio::time::timeout(fake::DEADLOCK_TEST_BOUND,async {
            provider.started(4).await;provider.release("body-1");provider.completed(1).await;
            for id in ["body-0","body-2","body-3"] {provider.release(id);}
            let (result,completed)=task.await.unwrap();assert!(result.is_err());
            assert_eq!(completed.into_iter().collect::<BTreeSet<_>>(),[0,2,3].into_iter().map(|i|sources[i].hash.clone()).collect());
        }).await.expect("failed hydration did not drain its siblings");
        assert_eq!(provider.inner.read_attempts("body-4"),0);assert_eq!(provider.inner.read_attempts("body-5"),0);
        let cas=crate::asset_repository::PayloadCas::new(f.directory_b.path()).unwrap();
        for index in [0,2,3] {assert!(cas.stat_object(&sources[index].hash).unwrap().is_some());}
    });
}

#[test]
fn standalone_connections_follow_display_rank_before_background_work() {
    let f=CycleFixture::new();
    let provider=Arc::new(DownloadProvider::new(4,&[]));
    let mut sources=sources(&f,&provider,4);
    sources[1].connection_id="other-receiver".into();
    register(f.directory_b.path(),&sources[1]).unwrap();
    let first=connection(&f,provider.clone(),None);
    let mut second=connection(&f,provider.clone(),None);
    second.stored.id="other-receiver".into();
    let _first=install_test_source_connection(f.directory_b.path(),Arc::new(first)).unwrap();
    let _second=install_test_source_connection(f.directory_b.path(),Arc::new(second)).unwrap();
    let hashes=sources.iter().map(|source|source.hash.clone()).collect::<Vec<_>>();
    let priority=hashes[..3].iter().cloned().collect();
    let mut completed=Vec::new();
    let unavailable=hydrate_registered_many(f.directory_b.path(),&hashes,&priority,None,&||Ok(()),&mut |hash,_|completed.push(hash.to_owned())).unwrap();
    assert!(unavailable.is_empty());
    assert_eq!(completed,hashes);
    let cas=crate::asset_repository::PayloadCas::new(f.directory_b.path()).unwrap();
    for (index,source) in sources.iter().enumerate() {
        assert_eq!(cas.read_object(&source.hash).unwrap().unwrap(),format!("synthetic standalone hydration {index}").into_bytes());
    }
}
