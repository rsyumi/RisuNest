use super::*;
use crate::external_storage::{contract::*, fake, lww_residency::PackedSource};
use crate::external_storage::capabilities::Capabilities;
use std::sync::{Arc, Mutex};

pub(crate) struct DownloadProvider {
    pub inner: fake::FakeProvider,
    pub width: usize,
    gates: BTreeMap<String, Arc<tokio::sync::Semaphore>>,
    state: Mutex<(usize, usize, Vec<String>, Vec<String>)>,
    changed: tokio::sync::Notify,
}
impl DownloadProvider {
    pub fn new(width: usize, held: &[&str]) -> Self {
        Self { inner: fake::FakeProvider::new(true), width,
            gates: held.iter().map(|id|(id.to_string(),Arc::new(tokio::sync::Semaphore::new(0)))).collect(),
            state: Mutex::new((0,0,Vec::new(),Vec::new())), changed:tokio::sync::Notify::new() }
    }
    pub fn release(&self,id:&str) {self.gates[id].add_permits(1);}
    pub async fn started(&self,count:usize) {
        loop {let changed=self.changed.notified();if self.state.lock().unwrap().2.len()>=count {break;}changed.await;}
    }
    pub async fn completed(&self,count:usize) {
        loop {let changed=self.changed.notified();if self.state.lock().unwrap().3.len()>=count {break;}changed.await;}
    }
    pub fn peak(&self)->usize {self.state.lock().unwrap().1}
}
struct Active<'a>(&'a DownloadProvider);
impl Drop for Active<'_> {fn drop(&mut self) {self.0.state.lock().unwrap().0-=1;}}
impl Provider for DownloadProvider {
    fn transfer_concurrency(&self)->usize {self.width}
    fn read_object<'a>(&'a self,r:&'a RepositoryHandle,l:&'a RemoteLocator,u:Option<&'a VersionToken>,s:&'a mut dyn TransferSink,c:&'a Cancellation)->ProviderFuture<'a,ReadReceipt> {
        Box::pin(async move {
            if !l.object.starts_with("pack-") && !l.object.starts_with("body-") {return self.inner.read_object(r,l,u,s,c).await;}
            {let mut state=self.state.lock().unwrap();state.0+=1;state.1=state.1.max(state.0);state.2.push(l.object.clone());}
            let _active=Active(self);self.changed.notify_waiters();
            if let Some(gate)=self.gates.get(&l.object) {gate.acquire().await.unwrap().forget();}
            let result=self.inner.read_object(r,l,u,s,c).await;
            self.state.lock().unwrap().3.push(l.object.clone());self.changed.notify_waiters();result
        })
    }
    fn open_repository<'a>(&'a self,c:&'a ConnectionConfig,s:&'a SecretRef,m:OpenMode,x:&'a Cancellation)->ProviderFuture<'a,(RepositoryHandle,Capabilities)> {self.inner.open_repository(c,s,m,x)}
    fn begin_upload<'a>(&'a self,r:&'a RepositoryHandle,i:&'a ObjectIntent,c:&'a Cancellation)->ProviderFuture<'a,Option<ResumeState>> {self.inner.begin_upload(r,i,c)}
    fn create_object<'a>(&'a self,r:&'a RepositoryHandle,i:&'a ObjectIntent,s:&'a dyn TransferSource,u:Option<&'a ResumeState>,c:&'a Cancellation)->ProviderFuture<'a,ObjectReceipt> {self.inner.create_object(r,i,s,u,c)}
    fn compare_exchange_head<'a>(&'a self,r:&'a RepositoryHandle,l:&'a RemoteLocator,e:&'a ExpectedHead,h:&'a HeadBytes,c:&'a Cancellation)->ProviderFuture<'a,HeadReceipt> {self.inner.compare_exchange_head(r,l,e,h,c)}
    fn replace_head<'a>(&'a self,r:&'a RepositoryHandle,l:&'a RemoteLocator,h:&'a HeadBytes,c:&'a Cancellation)->ProviderFuture<'a,HeadReceipt> {self.inner.replace_head(r,l,h,c)}
    fn list_objects<'a>(&'a self,r:&'a RepositoryHandle,o:Collection,s:Option<&'a str>,n:u16,c:&'a Cancellation)->ProviderFuture<'a,ObjectPage> {self.inner.list_objects(r,o,s,n,c)}
    fn delete_object<'a>(&'a self,r:&'a RepositoryHandle,l:&'a RemoteLocator,c:&'a Cancellation)->ProviderFuture<'a,()> {self.inner.delete_object(r,l,c)}
    fn reconcile_upload<'a>(&'a self,r:&'a RepositoryHandle,i:&'a ObjectIntent,u:Option<&'a ResumeState>,c:&'a Cancellation)->ProviderFuture<'a,UploadResolution> {self.inner.reconcile_upload(r,i,u,c)}
    fn lookup_metadata<'a>(&'a self,r:&'a RepositoryHandle,i:&'a ObjectIntent,l:Option<&'a RemoteLocator>,c:&'a Cancellation)->ProviderFuture<'a,Option<ObjectReceipt>> {self.inner.lookup_metadata(r,i,l,c)}
    fn head_locator(&self,r:&RepositoryHandle)->Result<RemoteLocator> {self.inner.head_locator(r)}
}

fn fixture(provider:&DownloadProvider,count:usize)->(BTreeMap<String,RemoteObject>,Vec<PackedSource>,Vec<Vec<u8>>) {
    let repository=fake::repository();let key=derive_key(&[7;32],&repository.repository_id,"data").unwrap();
    let mut remotes=BTreeMap::new();let mut sources=Vec::new();let mut expected=Vec::new();
    for index in 0..count {
        let id=format!("pack-{index}");let bytes=format!("synthetic body {index}").into_bytes();let hash:[u8;32]=Sha256::digest(&bytes).into();
        let mut plaintext=Vec::new();let stored_length=pack::write_entry(&mut plaintext,&pack::Chunk{hash,bytes:bytes.clone()}).unwrap();
        let header=wire::PublicObjectHeader::new(repository.repository_id.clone(),id.clone(),wire::ObjectRole::Pack,plaintext.len() as u64).unwrap();
        let mut sealed=Vec::new();wire::seal_envelope(&mut std::io::Cursor::new(&plaintext),&mut sealed,&key,&header).unwrap();
        let stored=wire::StoredObject{header,locator:wire::WireLocator{connection_identity:repository.connection_identity.clone(),collection:None,object:id.clone()},
            ciphertext_length:sealed.len() as u64,ciphertext_sha256:Sha256::digest(&sealed).into(),plaintext_length:plaintext.len() as u64,plaintext_sha256:Sha256::digest(&plaintext).into()};
        provider.inner.seed(&id,ObjectRole::Pack,sealed);
        remotes.insert(id.clone(),RemoteObject::from_stored(&stored,&repository).unwrap());
        let mut catalog=stored.clone();catalog.header=wire::PublicObjectHeader::new(repository.repository_id.clone(),format!("catalog-{index}"),wire::ObjectRole::Catalog,catalog.plaintext_length).unwrap();
        catalog.locator.object=catalog.header.object_id.clone();catalog.ciphertext_length=wire::envelope_length(&catalog.header).unwrap();
        sources.push(PackedSource{hash:hex::encode(hash),byte_length:bytes.len() as u64,library_id:"synthetic".into(),connection_id:"synthetic".into(),connection_root:PathBuf::new(),
            protected_snapshot:"synthetic".into(),catalog,packs:vec![stored],chunks:vec![wire::StoredChunk{pack_id:id,offset:0,stored_length,plaintext_length:bytes.len() as u64,plaintext_sha256:hash}]});
        expected.push(bytes);
    }
    (remotes,sources,expected)
}
fn run(future:impl std::future::Future<Output=()>) {tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
    tokio::time::timeout(std::time::Duration::from_secs(20),future).await.expect("download test deadlocked");
});}

#[test]
fn open_packs_downloads_four_ciphertexts_and_preserves_identity_after_reverse_completion() {run(async {
    let stage=tempfile::tempdir().unwrap();let provider=Arc::new(DownloadProvider::new(4,&["pack-0","pack-1","pack-2","pack-3"]));
    let (packs,_,_)=fixture(&provider,6);let path=stage.path().to_owned();let worker=provider.clone();
    let expected=packs.clone();let progress=PhaseProgress::silent();let observed=progress.clone();
    let task=tokio::spawn(async move {open_packs(&packs,&[7;32],&path,worker.as_ref(),&fake::repository(),&observed,&Cancellation::default()).await});
    provider.started(4).await;assert_eq!(provider.peak(),4);
    for id in ["pack-3","pack-2","pack-1"] {provider.release(id);}
    provider.completed(3).await;
    assert!(!stage.path().join("plaintext").exists(),"out-of-order workers only stage ciphertext");
    assert_eq!(fs::read_dir(stage.path().join("downloads")).unwrap().filter_map(|entry|entry.ok()).filter(|entry|entry.path().extension().is_some_and(|ext|ext=="cipher")).count(),3);
    assert_eq!(provider.state.lock().unwrap().2.len(),4,"blocked owner must not start another batch");
    provider.release("pack-0");let paths=task.await.unwrap().unwrap();
    assert_eq!(paths.len(),6);
    assert_eq!(progress.read().items,6);assert_eq!(progress.read().bytes,expected.values().map(|object|object.receipt.byte_length).sum::<u64>());
    for (id,path) in paths {let object=&expected[&id];assert!(verify(&path,object.plaintext_length,&object.plaintext_sha256).unwrap());}
    assert_eq!(provider.peak(),4);
});}

#[test]
fn open_packs_reports_bytes_before_one_pack_finishes() {run(async {
    for exporting in [false,true] {
    let stage=tempfile::tempdir().unwrap();let provider=Arc::new(DownloadProvider::new(1,&[]));
    let (packs,sources,bodies)=fixture(&provider,1);let expected=packs["pack-0"].receipt.byte_length;
    let barrier=provider.inner.arm_partial_read_barrier("pack-0",16).unwrap();
    let delivered=Arc::new(std::sync::atomic::AtomicU64::new(0));let sink=delivered.clone();
    let progress=PhaseProgress::new(move |reading|{sink.store(reading.bytes,std::sync::atomic::Ordering::Relaxed);});
    if !exporting { progress.plan(1, expected); }
    let observed=progress.clone();let path=stage.path().to_owned();let worker=provider.clone();
    let task=tokio::spawn(async move {
        if exporting {
            let source=&sources[0];
            let body=download_packed_body_file_observed(&source.hash,source.byte_length,source.chunks.clone(),source.packs.clone(),&path,&[7;32],worker.as_ref(),&fake::repository(),&observed,&Cancellation::default()).await?;
            assert_eq!(fs::read(body).unwrap(),bodies[0]);
        } else {open_packs(&packs,&[7;32],&path,worker.as_ref(),&fake::repository(),&observed,&Cancellation::default()).await?;}
        Ok::<_,ProviderError>(())
    });
    barrier.reached.notified().await;
    assert_eq!((progress.read().items,progress.read().bytes),(0,16));
    assert_eq!(delivered.load(std::sync::atomic::Ordering::Relaxed),16);
    assert!(!task.is_finished());
    barrier.release();task.await.unwrap().unwrap();
    progress.flush();
    assert_eq!((progress.read().items,progress.read().bytes),(1,expected));
    assert_eq!(delivered.load(std::sync::atomic::Ordering::Relaxed),expected);
    }
});}

#[test]
fn ciphertext_batch_deduplicates_destinations_and_stops_at_a_slow_consumer() {run(async {
    let stage=tempfile::tempdir().unwrap();let provider=DownloadProvider::new(4,&[]);let (packs,_,_)=fixture(&provider,4);
    let repository=fake::repository();let cancel=Cancellation::default();let objects=vec![&packs["pack-0"],&packs["pack-0"],&packs["pack-1"],&packs["pack-2"]];
    let mut downloads=stage_ciphertexts(&objects,stage.path(),&provider,&repository,Durability::Scratch,None,&cancel).unwrap();
    let (first,result)=downloads.next().await.unwrap();assert_eq!(first.object_id,"pack-0");assert!(result.unwrap().unwrap().exists());
    assert!(!stage.path().join("plaintext").exists());
    let reads=provider.inner.read_count();tokio::task::yield_now().await;assert_eq!(provider.inner.read_count(),reads);
    while let Some((_,result))=downloads.next().await {assert!(result.unwrap().unwrap().exists());}
    assert_eq!(provider.inner.read_attempts("pack-0"),1);assert_eq!(provider.inner.read_attempts("pack-3"),0);
});}

#[test]
fn turnover_keeps_placed_prefix_and_verified_siblings_on_failure_then_resumes() {run(async {
    let stage=tempfile::tempdir().unwrap();let provider=Arc::new(DownloadProvider::new(4,&["pack-0","pack-1","pack-2","pack-3"]));
    let (_,sources,expected)=fixture(&provider,6);provider.inner.fail_read("pack-1",ErrorKind::Transient);
    let path=stage.path().to_owned();let worker=provider.clone();let copied=sources.clone();
    let task=tokio::spawn(async move {download_packed_body_files(&copied,&path,&[7;32],worker.as_ref(),&fake::repository(),&Cancellation::default()).await});
    provider.started(4).await;
    for id in ["pack-3","pack-2"] {provider.release(id);}provider.completed(2).await;
    provider.release("pack-0");provider.completed(3).await;provider.release("pack-1");
    assert_eq!(task.await.unwrap().unwrap_err().kind,ErrorKind::Transient);
    assert!(marker_path(stage.path(),"pack-0").unwrap().exists());
    assert_eq!(provider.inner.read_attempts("pack-4"),0);
    provider.release("pack-1");
    let paths=download_packed_body_files(&sources,stage.path(),&[7;32],provider.as_ref(),&fake::repository(),&Cancellation::default()).await.unwrap();
    for (source,bytes) in sources.iter().zip(expected) {assert_eq!(fs::read(&paths[&source.hash]).unwrap(),bytes);}
    for path in paths.values() {assert_ne!(path.parent(),Some(stage.path()),"a body was staged in the root");}
    assert_eq!(provider.inner.read_attempts("pack-0"),1);assert_eq!(provider.inner.read_attempts("pack-2"),1);assert_eq!(provider.inner.read_attempts("pack-3"),1);
});}

#[test]
fn restore_plan_prefetches_four_but_applies_one_pack_per_owner_turn() {run(async {
    let stage=tempfile::tempdir().unwrap();let provider=Arc::new(DownloadProvider::new(4,&["pack-0","pack-1","pack-2","pack-3"]));
    let (_,sources,expected)=fixture(&provider,6);let repository=fake::repository();let mut plan=RestoreBodyPlan::new(stage.path()).unwrap();
    for source in &sources {plan.push(source,false,&repository).unwrap();}plan.seal().unwrap();
    let worker=provider.clone();let task=tokio::spawn(async move {
        plan.next_pack(&[7;32],worker.as_ref(),&repository,&PhaseProgress::silent(),&Cancellation::default()).await.unwrap();plan
    });
    provider.started(4).await;
    for id in ["pack-3","pack-2","pack-1","pack-0"] {provider.release(id);}
    let mut plan=task.await.unwrap();assert_eq!(provider.peak(),4);
    assert_eq!(plan.db.query_row("SELECT count(*) FROM packs WHERE done=1",[],|r|r.get::<_,i64>(0)).unwrap(),1);
    assert_eq!(fs::read_dir(plan.directory.join("downloads")).unwrap().filter_map(|entry|entry.ok()).filter(|entry|entry.path().extension().is_some_and(|ext|ext=="cipher")).count(),3);
    assert_eq!(provider.inner.read_attempts("pack-4"),0);
    let (hash,path)=plan.ready().unwrap().unwrap();assert_eq!(hash,sources[0].hash);assert_eq!(fs::read(path).unwrap(),expected[0]);plan.settled(&hash).unwrap();
    for index in 1..6 {
        assert!(plan.next_pack(&[7;32],provider.as_ref(),&fake::repository(),&PhaseProgress::silent(),&Cancellation::default()).await.unwrap());
        let (hash,path)=plan.ready().unwrap().unwrap();assert_eq!(hash,sources[index].hash);assert_eq!(fs::read(path).unwrap(),expected[index]);plan.settled(&hash).unwrap();
    }
    assert!(!plan.next_pack(&[7;32],provider.as_ref(),&fake::repository(),&PhaseProgress::silent(),&Cancellation::default()).await.unwrap());plan.finish().unwrap();
});}
