use super::{
    lww_tests::{local, put_asset, LocalServerFixture},
    residency::Residency,
};
use crate::{
    asset_repository::{
        job_pins::{CasJobKind, CasObjectRole, DurableCasJob},
        PayloadCas,
    },
    persistent_store::{PersistentStore, PluginStorageMutation, WorkingSetCommit},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

#[test]
fn present_bulk_hydration_completes_without_any_body_open_or_hash() {
    use crate::asset_repository::body_io::{reset_body_io, take_body_io};
    let (_root, mut store) = local();
    let hashes = (0..3).map(|index| put_asset(&mut store, &format!("assets/present-{index}.png"), &vec![index; 100_000]).object_hash.unwrap()).collect::<Vec<_>>();
    let mut session = super::residency::HydrationSession::new(store.repository_root(), None).unwrap();
    reset_body_io(); crate::persistent_store::hash_work::reset_hash_work();
    let mut completed = Vec::new();
    assert!(session.hydrate_many_outcomes(&hashes, &|| Ok(()), |hash,outcome| completed.push((hash.to_owned(),outcome))).unwrap().is_empty());
    let work = take_body_io(); let hashes_work = crate::persistent_store::hash_work::take_hash_work();
    assert!(work.complete()); assert!(work.domains.is_empty());
    assert_eq!((work.stat_requests, work.batch_stat_requests), (0, 1));
    assert_eq!(completed.len(), hashes.len());
    assert!(completed.iter().all(|(_,outcome)| *outcome == super::residency::HydrationOutcome::AlreadyLocal));
    assert!(hashes_work.domains.is_empty()); assert!(hashes_work.incomplete.is_empty());
}

/// Received remote bodies are retained under the epoch this device last confirmed custody
/// under, so a receive of many pages reads the server head once, and again only when the
/// server has left that epoch.
#[test]
fn received_pages_retain_under_the_confirmed_epoch_and_read_the_head_only_when_it_moved() {
    use super::lww_tests::{drain_publications, receive_available};
    use std::sync::atomic::AtomicUsize;
    let heads = Arc::new(AtomicUsize::new(0));
    let observed = heads.clone();
    let server = LocalServerFixture::with_router(move |router| router.layer(axum::middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| {
            let observed = observed.clone();
            async move {
                if request.method() == axum::http::Method::GET && request.uri().path().ends_with("/session") {
                    observed.fetch_add(1, Ordering::Relaxed);
                }
                next.run(request).await
            }
        },
    )));
    let (_a_root, mut a) = local();
    let (_b_root, mut b) = local();
    let ca = server.client(&a);
    let cb = server.client(&b);
    let publish = |a: &mut PersistentStore, prefix: &str, count: usize| {
        let hashes = (0..count).map(|index| put_asset(a, &format!("assets/{prefix}-{index}.png"), format!("synthetic {prefix} {index}").as_bytes()).object_hash.unwrap()).collect::<Vec<_>>();
        drain_publications(&ca, a, &[]).unwrap();
        hashes
    };
    let retained = |b: &PersistentStore, hashes: &[String]| {
        let sizes = Residency::open(b.repository_root()).unwrap().active_sizes(hashes).unwrap();
        hashes.iter().zip(sizes).all(|(hash, size)| size.is_some() && b.asset_object_byte_size(hash).unwrap() == size)
    };

    let first = publish(&mut a, "first", 300);
    heads.store(0, Ordering::Relaxed);
    receive_available(&cb, &mut b, &[]).unwrap();
    assert_eq!(heads.load(Ordering::Relaxed), 1);
    assert!(retained(&b, &first));

    // Custody this device last confirmed under an epoch the server has since left.
    let config = b.server_stored_config().unwrap().unwrap();
    let earlier = Residency::context_id(&config, "an-earlier-epoch");
    let db = rusqlite::Connection::open(b.repository_root().join("server-sync").join("asset-residency.sqlite")).unwrap();
    db.execute("INSERT INTO contexts VALUES(?1,?2,?3,'an-earlier-epoch',?4)", rusqlite::params![earlier, config.library_id, config.device_id, serde_json::to_string(&config).unwrap()]).unwrap();
    db.execute("INSERT INTO objects SELECT ?1,hash,size,retention_id,'active',(SELECT MAX(confirmation_order)+1 FROM objects) FROM objects LIMIT 1", [&earlier]).unwrap();
    drop(db);
    let second = publish(&mut a, "second", 3);
    heads.store(0, Ordering::Relaxed);
    receive_available(&cb, &mut b, &[]).unwrap();
    assert_eq!(heads.load(Ordering::Relaxed), 1);
    assert!(retained(&b, &second));
}

#[test]
fn server_hydration_prepares_downloaded_bodies_with_four_joined_workers() {
    use crate::asset_repository::body_io;
    let server = LocalServerFixture::new();
    let (_root, mut store) = local();
    let core = server.client(&store);
    let bodies = (0_u8..8).map(|index| vec![index; 80 * 1024]).collect::<Vec<_>>();
    let hashes = bodies.iter().enumerate().map(|(index, body)| {
        put_asset(&mut store, &format!("assets/parallel-{index}.png"), body).object_hash.unwrap()
    }).collect::<Vec<_>>();
    super::lww_tests::drain_publications(&core, &mut store, &[]).unwrap();
    store.asset_residency_set_policy(super::residency::AssetPolicy::Remote, || Ok(())).unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    body_io::reset_body_io();
    for hash in &hashes { body_io::register_object_purpose(hash, body_io::BodyPurpose::Asset); }
    let mut session = super::residency::HydrationSession::new(store.repository_root(), None).unwrap();
    assert!(session.hydrate_many(&hashes, &|| Ok(())).unwrap().is_empty());
    let observed = body_io::take_body_io();
    assert_eq!(observed.worker_scopes_started, 4);
    assert_eq!(observed.worker_scopes_settled, 4);
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    for (hash, body) in hashes.iter().zip(bodies) {
        assert_eq!(cas.read_object(hash).unwrap(), Some(body));
        assert_eq!(store.asset_object_byte_size(hash).unwrap(), Some(80 * 1024));
    }
}

/// A body fetched from server custody, or already here when asked for, has its
/// catalog row afterwards, whatever had taken the row away before.
#[test]
fn server_hydration_registers_every_body_it_fetches_or_finds_local() {
    use super::residency::{HydrationOutcome, HydrationSession};
    let server = LocalServerFixture::new();
    let (_root, mut store) = local();
    let core = server.client(&store);
    let present = vec![61; 128 * 1024 + 1];
    let fetched = vec![67; 128 * 1024 + 1];
    let present_hash = put_asset(&mut store, "assets/catalog-present.png", &present).object_hash.unwrap();
    let fetched_hash = put_asset(&mut store, "assets/catalog-fetched.png", &fetched).object_hash.unwrap();
    super::lww_tests::drain_publications(&core, &mut store, &[]).unwrap();
    store.asset_residency_set_policy(super::residency::AssetPolicy::Remote, || Ok(())).unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    let mut session = HydrationSession::new(store.repository_root(), None).unwrap();
    assert!(session.hydrate_many(std::slice::from_ref(&present_hash), &|| Ok(())).unwrap().is_empty());
    assert!(cas.stat_object(&present_hash).unwrap().is_some());
    assert!(cas.stat_object(&fetched_hash).unwrap().is_none());
    // What a catalog rolled back to an older snapshot no longer has.
    let catalog = rusqlite::Connection::open(store.repository_root().join("persistent").join(crate::persistent_store::DATABASE_FILE)).unwrap();
    for hash in [&present_hash, &fetched_hash] {
        catalog.execute("DELETE FROM asset_objects WHERE object_hash=?1", [hash]).unwrap();
    }
    let mut outcomes = Vec::new();
    assert!(session.hydrate_many_outcomes(&[present_hash.clone(), fetched_hash.clone()], &|| Ok(()),
        |hash, outcome| outcomes.push((hash.to_owned(), outcome))).unwrap().is_empty());
    assert_eq!(outcomes, [(present_hash.clone(), HydrationOutcome::AlreadyLocal), (fetched_hash.clone(), HydrationOutcome::Downloaded)]);
    assert_eq!(store.asset_object_byte_size(&present_hash).unwrap(), Some(present.len() as u64));
    assert_eq!(store.asset_object_byte_size(&fetched_hash).unwrap(), Some(fetched.len() as u64));
    store.asset_gc_dry_run(1024, None, crate::external_storage::runtime::now_ms() as i64, 0).unwrap();
}

/// Local, server-held and missing bodies are told apart by one batched presence check, and a
/// link in place of a shard folder fails the status and the download instead of being counted.
#[test]
fn the_status_and_download_check_presence_in_one_pass_and_refuse_a_linked_shard() {
    use crate::asset_repository::body_io::{reset_body_io, take_body_io};
    fn link_directory(target: &std::path::Path, link: &std::path::Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            assert!(std::process::Command::new("cmd").creation_flags(0x08000000).args(["/C", "mklink", "/J"])
                .arg(link).arg(target).output().unwrap().status.success());
        }
    }
    let server = LocalServerFixture::new();
    let (_root, mut store) = local();
    let core = server.client(&store);
    let held = vec![81; 64 * 1024];
    let held_hash = put_asset(&mut store, "assets/status-held.png", &held).object_hash.unwrap();
    super::lww_tests::drain_publications(&core, &mut store, &[]).unwrap();
    store.asset_residency_set_policy(super::residency::AssetPolicy::Remote, || Ok(())).unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    let kept = b"synthetic body kept on this device".to_vec();
    let kept_hash = put_asset(&mut store, "assets/status-kept.png", &kept).object_hash.unwrap();
    let lost = b"synthetic body no storage holds".to_vec();
    let lost_hash = put_asset(&mut store, "assets/status-lost.png", &lost).object_hash.unwrap();
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    cas.unlink_exact_object(&lost_hash, lost.len() as u64, &crate::asset_repository::object_physical_key(&lost_hash)).unwrap();
    let counts = |status: &serde_json::Value| ["localBytes", "serverObjects", "serverBytes", "unavailableObjects"].map(|field| status[field].as_u64().unwrap());

    reset_body_io();
    let status = serde_json::to_value(store.asset_residency_status().unwrap()).unwrap();
    let work = take_body_io();
    assert_eq!((work.stat_requests, work.batch_stat_requests), (0, 1));
    assert_eq!(counts(&status), [kept.len() as u64, 1, held.len() as u64, 1]);

    let shard = store.repository_root().join("assets").join("objects").join(&kept_hash[..2]);
    let outside = tempfile::tempdir().unwrap();
    let moved = outside.path().join("shard");
    std::fs::rename(&shard, &moved).unwrap();
    link_directory(&moved, &shard);
    assert!(store.asset_residency_status().is_err());
    assert!(store.asset_residency_download_remote(None, None, None, || Ok(())).is_err());
    assert!(cas.stat_object(&held_hash).unwrap().is_none(), "a refused download fetches nothing");
    #[cfg(unix)]
    std::fs::remove_file(&shard).unwrap();
    #[cfg(windows)]
    std::fs::remove_dir(&shard).unwrap();
    std::fs::rename(&moved, &shard).unwrap();

    let downloaded = serde_json::to_value(store.asset_residency_download_remote(None, None, None, || Ok(())).unwrap()).unwrap();
    assert_eq!(cas.stat_object(&held_hash).unwrap(), Some(held.len() as u64));
    assert_eq!(counts(&downloaded), [(kept.len() + held.len()) as u64, 0, 0, 1]);
}

/// Offloading keeps every catalog row, so only the device body figure drops,
/// by what the residency status then counts as held by the server alone.
#[test]
fn offloading_lowers_the_device_body_bytes_and_keeps_the_catalog() {
    let server = LocalServerFixture::new();
    let (_root, mut store) = local();
    let core = server.client(&store);
    let first = vec![71; 128 * 1024 + 1];
    let second = vec![73; 64 * 1024];
    put_asset(&mut store, "assets/offload-first.png", &first);
    put_asset(&mut store, "assets/offload-second.png", &second);
    super::lww_tests::drain_publications(&core, &mut store, &[]).unwrap();
    let offloaded = (first.len() + second.len()) as u64;
    let before = store.storage_stats().unwrap();
    assert_eq!(before.missing_asset_bodies.count, 0);
    assert!(before.asset_bodies.bytes >= offloaded);

    store.asset_residency_set_policy(super::residency::AssetPolicy::Remote, || Ok(())).unwrap();
    assert_eq!(store.asset_residency_evict(|| Ok(())).unwrap().evicted_bytes, offloaded);

    let after = store.storage_stats().unwrap();
    assert_eq!(after.asset_objects, before.asset_objects);
    assert_eq!(after.asset_bodies.count, before.asset_bodies.count - 2);
    assert_eq!(after.asset_bodies.bytes, before.asset_bodies.bytes - offloaded);
    assert_eq!((after.missing_asset_bodies.count, after.missing_asset_bodies.bytes), (2, offloaded));
    let status = serde_json::to_value(store.asset_residency_status().unwrap()).unwrap();
    assert_eq!(status["serverBytes"], offloaded);
}

#[test]
fn selected_archive_priority_uses_native_metadata_without_reading_the_archive() {
    use crate::asset_repository::body_io::{reset_body_io,take_body_io};
    let (_root,mut store)=local();
    let hash=put_asset(&mut store,"assets/archived-priority.png",b"synthetic archive priority").object_hash.unwrap();
    store.commit(&WorkingSetCommit {
        expected_revision:store.revision().unwrap(),
        add_character:Some(serde_json::json!({"chaId":"archived-priority","type":"character","name":"Synthetic","image":"assets/archived-priority.png","chats":[]})),
        ..Default::default()
    }).unwrap();
    store.archive_character("archived-priority",store.revision().unwrap(),1).unwrap();
    reset_body_io(); crate::persistent_store::hash_work::reset_hash_work();
    let hashes=store.selected_character_asset_hashes("archived-priority").unwrap();
    let work=take_body_io(); let hashes_work=crate::persistent_store::hash_work::take_hash_work();
    assert!(hashes.contains(&hash)); assert!(hashes.len()>1);
    assert!(work.complete()); assert!(work.domains.is_empty()); assert!(hashes_work.domains.is_empty());
}

#[test]
fn hydration_rejects_stale_authority_or_cancellation_before_opening_any_body() {
    use crate::asset_repository::body_io::{reset_body_io,take_body_io};
    let server=LocalServerFixture::new(); let (_root,mut store)=local();
    let core=server.client(&store);
    put_asset(&mut store,"assets/authority-hydration.png",b"synthetic authority hydration");
    super::lww_tests::drain_publications(&core,&mut store,&[]).unwrap();
    let _residency=Residency::open(store.repository_root()).unwrap();
    let mut request=super::lww_tests::header(&store);
    request.binding_authority=(request.binding_authority.0+1).into();
    reset_body_io();
    let result=super::commands::hydrate_binding_assets(&store,&request,Arc::new(AtomicBool::new(false)),None,||panic!("stale hydration completed"));
    assert_eq!(result.unwrap_err().code,"binding-authority-changed");
    let work=take_body_io(); assert!(work.complete()); assert!(work.domains.is_empty());
    let result=super::commands::hydrate_binding_assets(&store,&super::lww_tests::header(&store),Arc::new(AtomicBool::new(true)),None,||panic!("cancelled hydration completed"));
    assert_eq!(result.unwrap_err().code,"cancelled");
    let work=take_body_io(); assert!(work.complete()); assert!(work.domains.is_empty());
}

#[test]
fn selected_character_assets_finish_before_uncapped_catalog_hydration() {
    let server = LocalServerFixture::new();
    let (_root, mut store) = local();
    let core = server.client(&store);
    let hashes = (0..130).map(|index| put_asset(&mut store, &format!("assets/priority-{index}.png"), format!("synthetic priority {index}").as_bytes()).object_hash.unwrap()).collect::<Vec<_>>();
    super::lww_tests::drain_publications(&core, &mut store, &[]).unwrap();
    store.asset_residency_set_policy(super::residency::AssetPolicy::Remote, || Ok(())).unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    // Make every body remote before assigning the representative image.
    store.commit(&WorkingSetCommit {
        expected_revision: store.revision().unwrap(),
        add_character: Some(serde_json::json!({"chaId":"selected-priority","type":"character","name":"Synthetic","image":"assets/priority-129.png","additionalAssets":[["extra","assets/priority-128.png","png"]],"chats":[]})),
        ..Default::default()
    }).unwrap();
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    assert_eq!(store.selected_character_asset_hashes("selected-priority").unwrap(), [hashes[128].clone(),hashes[129].clone()].into_iter().collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>());
    assert!(store.selected_character_asset_hashes("missing-character").unwrap().is_empty());
    let visits = std::cell::Cell::new(0);
    store.hydrate_registered_remote_assets_prioritized(None, Some("selected-priority"), || Ok(()), || {
        visits.set(visits.get()+1);
        if visits.get() == 2 {
            assert!(cas.stat_object(&hashes[128]).unwrap().is_some());
            assert!(cas.stat_object(&hashes[129]).unwrap().is_some());
            assert!(hashes[..128].iter().all(|hash| cas.stat_object(hash).unwrap().is_none()));
        }
    }).unwrap();
    assert_eq!(visits.get(),130);
    assert!(hashes.iter().all(|hash| cas.stat_object(hash).unwrap().is_some()));
}

#[test]
fn switching_to_full_policy_hydrates_selected_references_before_other_characters() {
    let server=LocalServerFixture::new(); let (_root,mut store)=local();
    let core=server.client(&store);
    let mut aliases=[
        put_asset(&mut store,"assets/policy-first.png",b"first synthetic policy priority"),
        put_asset(&mut store,"assets/policy-second.png",b"second synthetic policy priority"),
    ];
    aliases.sort_by(|a,b| a.object_hash.cmp(&b.object_hash));
    super::lww_tests::drain_publications(&core,&mut store,&[]).unwrap();
    store.asset_residency_set_policy(super::residency::AssetPolicy::Remote,||Ok(())).unwrap();
    store.asset_residency_evict(||Ok(())).unwrap();
    // Start with remote bodies, before the new character references are hydrated.
    for (character,alias) in [("policy-other",&aliases[0]),("policy-selected",&aliases[1])] {
        store.commit(&WorkingSetCommit {
            expected_revision:store.revision().unwrap(),
            add_character:Some(serde_json::json!({"chaId":character,"type":"character","name":"Synthetic","image":alias.key,"chats":[]})),
            ..Default::default()
        }).unwrap();
    }
    let cas=PayloadCas::new(store.repository_root()).unwrap();
    let selected=aliases[1].object_hash.as_ref().unwrap(); let other=aliases[0].object_hash.as_ref().unwrap();
    assert!(cas.stat_object(selected).unwrap().is_none()); assert!(cas.stat_object(other).unwrap().is_none());
    let prioritized=std::cell::Cell::new(false);
    store.asset_residency_set_policy_prioritized(super::residency::AssetPolicy::Full,None,Some("policy-selected"),||{
        if cas.stat_object(selected)?.is_some() && cas.stat_object(other)?.is_none() { prioritized.set(true); }
        Ok(())
    }).unwrap();
    assert!(prioritized.get()); assert!(cas.stat_object(selected).unwrap().is_some()); assert!(cas.stat_object(other).unwrap().is_some());
}

#[test]
fn selected_archived_owner_hydration_requires_only_the_existing_full_inventory() {
    use crate::{
        asset_repository::{job_pins::CasReleaseOutcome, owner_manifest_codec::{encode_owner_manifest,OwnerManifestEntry}},
        persistent_store::{AssetOwnerHead,AssetOwnerLocator},
    };
    let server=LocalServerFixture::new(); let (_root,mut store)=local();
    let core=server.client(&store);
    let asset=put_asset(&mut store,"assets/owner-archive.png",b"synthetic selected archived owner payload").object_hash.unwrap();
    let cas=PayloadCas::new(store.repository_root()).unwrap();
    let canonical=encode_owner_manifest(&[OwnerManifestEntry {
        tuple:["extra".into(),"assets/owner-archive.png".into(),"png".into()],
        payload_hash:Some(hex::decode(&asset).unwrap().try_into().unwrap()),
    }]).unwrap();
    let mut job=DurableCasJob::begin(store.repository_root(),"selected-archive-owner",CasJobKind::CardOrModuleContentImport,crate::asset_repository::job_pins::CasJobOwner::for_test(),1).unwrap();
    let manifest=job.prepare_reader(&cas,&mut canonical.as_slice(),CasObjectRole::OwnerManifest).unwrap();
    job.seal(&mut store,2).unwrap();
    let head=AssetOwnerHead::present(AssetOwnerLocator::CharacterAdditionalAssets { character_id:"selected-owner-archive".into() },manifest.content_hash.clone(),1);
    store.commit(&WorkingSetCommit {
        expected_revision:store.revision().unwrap(),
        add_character:Some(serde_json::json!({"chaId":"selected-owner-archive","type":"character","name":"Synthetic","additionalAssets":[["extra","assets/owner-archive.png","png"]],"localFuture":"synthetic local-only archive detail","chats":[]})),
        asset_owner_heads:Some(vec![head.clone()]),
        ..Default::default()
    }).unwrap();
    job.release(CasReleaseOutcome::Committed).unwrap();
    super::lww_tests::drain_publications(&core,&mut store,&[]).unwrap();
    store.archive_character("selected-owner-archive",store.revision().unwrap(),5).unwrap();
    super::lww_tests::drain_publications(&core,&mut store,&[]).unwrap();
    let generation=store.read_asset_owner_head(&head.owner,None).unwrap().unwrap();
    assert_eq!(generation.value,head);
    let selected=store.selected_character_asset_hashes("selected-owner-archive").unwrap();
    assert!(selected.contains(&manifest.content_hash)); assert!(selected.contains(&asset));
    let archive_unit=store.lww_read_unit_state(store.lww_binding_authority().unwrap(),None,100).unwrap().entries.into_iter().find(|entry|entry.key.components()==["archive","selected-owner-archive"]).unwrap();
    let risunest_sync_wire::unit::UnitValue::Object {descriptor,..}=archive_unit.value else {panic!("native archive must have a descriptor")};
    let metadata:serde_json::Value=serde_json::from_slice(&store.lww_object_body(&descriptor.object_hash).unwrap().unwrap()).unwrap();
    let shared=metadata["sharedObjectHash"].as_str().unwrap().to_owned();
    let local_roots=selected.iter().filter(|hash|**hash!=asset && **hash!=manifest.content_hash && **hash!=shared).cloned().collect::<Vec<_>>();
    assert_eq!(local_roots.len(),1);
    let local_archive=&local_roots[0];
    assert!(selected.contains(&shared)); assert_ne!(local_archive,&shared);
    let inventory=store.residency_inventory_classification_test(false).unwrap();
    for hash in [local_archive,&shared,&asset,&manifest.content_hash] {assert!(inventory.0.contains(hash));}
    assert!(!selected.contains(&descriptor.object_hash));
    assert!(cas.stat_object(&descriptor.object_hash).unwrap().is_none());
    store.asset_residency_set_policy(super::residency::AssetPolicy::Remote,||Ok(())).unwrap();
    store.asset_residency_evict(||Ok(())).unwrap();
    assert!(cas.stat_object(&asset).unwrap().is_none()); assert!(cas.stat_object(local_archive).unwrap().is_none());
    assert!(cas.stat_object(&manifest.content_hash).unwrap().is_some());
    let collected=store.asset_gc_delete_page(64,None,10_000,0).unwrap();
    assert!(!collected.report.deleted_hashes.contains(&shared));
    assert!(cas.stat_object(&shared).unwrap().is_none());
    assert!(Residency::open(store.repository_root()).unwrap().object(&shared,None).unwrap().is_some());
    store.asset_residency_set_policy_prioritized(super::residency::AssetPolicy::Full,None,Some("selected-owner-archive"),||Ok(())).unwrap();
    assert!(cas.stat_object(&asset).unwrap().is_some()); assert!(cas.stat_object(local_archive).unwrap().is_some());
    assert!(cas.stat_object(&manifest.content_hash).unwrap().is_some());
    assert!(cas.stat_object(&shared).unwrap().is_some());
    assert_eq!(store.read_asset_owner_head(&head.owner,None).unwrap().unwrap().value,head);
}

#[test]
fn selected_priority_does_not_hide_missing_required_assets_in_the_full_inventory() {
    let server=LocalServerFixture::new(); let (_root,mut store)=local();
    let _core=server.client(&store);
    let selected=put_asset(&mut store,"assets/selected-present.png",b"synthetic selected present payload");
    let missing=put_asset(&mut store,"assets/other-missing.png",b"synthetic required missing payload");
    for (character,alias) in [("selected-present",&selected),("other-missing",&missing)] {
        store.commit(&WorkingSetCommit {
            expected_revision:store.revision().unwrap(),
            add_character:Some(serde_json::json!({"chaId":character,"type":"character","name":"Synthetic","image":alias.key,"chats":[]})),
            ..Default::default()
        }).unwrap();
    }
    let cas=PayloadCas::new(store.repository_root()).unwrap();
    let hash=missing.object_hash.as_ref().unwrap();
    let size=cas.stat_object(hash).unwrap().unwrap();
    cas.unlink_exact_object(hash,size,&crate::asset_repository::object_physical_key(hash)).unwrap();
    assert!(store.residency_inventory_classification_test(false).unwrap().0.contains(hash));
    assert!(Residency::open(store.repository_root()).unwrap().object(hash,None).unwrap().is_none());
    store.asset_residency_set_policy(super::residency::AssetPolicy::Remote,||Ok(())).unwrap();
    let error=store.asset_residency_set_policy_prioritized(super::residency::AssetPolicy::Full,None,Some("selected-present"),||Ok(())).err().unwrap();
    assert_eq!(error.code,"required-asset-unavailable");
    assert!(cas.stat_object(selected.object_hash.as_ref().unwrap()).unwrap().is_some());
    assert!(cas.stat_object(hash).unwrap().is_none());
}

#[test]
fn cancelled_full_hydration_keeps_remote_proof_and_retries_without_a_partial_body() {
    let cancelled = Arc::new(AtomicBool::new(false));
    let armed = Arc::new(AtomicBool::new(false));
    let request_cancel = cancelled.clone();
    let request_armed = armed.clone();
    let server = LocalServerFixture::with_router(move |router| {
        router.layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let cancelled = request_cancel.clone();
                let armed = request_armed.clone();
                async move {
                    if (request.uri().path() == "/objects/transfer"
                        || request.uri().path().starts_with("/objects/"))
                        && armed.swap(false, Ordering::SeqCst)
                    {
                        cancelled.store(true, Ordering::SeqCst);
                        return axum::response::Response::builder()
                            .status(503)
                            .body(axum::body::Body::from("{\"error\":\"server-unavailable\"}"))
                            .unwrap();
                    }
                    next.run(request).await
                }
            },
        ))
    });
    let (_root, mut store) = local();
    let core = server.client(&store);
    let bytes = vec![43; 128 * 1024 + 1];
    let hash = put_asset(&mut store, "assets/recover.png", &bytes)
        .object_hash
        .unwrap();
    let request = super::lww_tests::header(&store);
    core.push(&mut store, &request, &[]).unwrap();
    store
        .asset_residency_set_policy(super::residency::AssetPolicy::Remote, || Ok(()))
        .unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    armed.store(true, Ordering::SeqCst);
    let result = store.asset_residency_set_policy_cancelled(
        super::residency::AssetPolicy::Full,
        Some(cancelled.clone()),
        || {
            if cancelled.load(Ordering::SeqCst) {
                Err(super::SyncError::new("cancelled", 409))
            } else {
                Ok(())
            }
        },
    );
    assert_eq!(result.err().unwrap().code, "cancelled");
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    assert!(cas.stat_object(&hash).unwrap().is_none());
    assert!(Residency::open(store.repository_root())
        .unwrap()
        .object(&hash, None)
        .unwrap()
        .is_some());
    cancelled.store(false, Ordering::SeqCst);
    store
        .asset_residency_set_policy(super::residency::AssetPolicy::Full, || Ok(()))
        .unwrap();
    assert_eq!(cas.read_object(&hash).unwrap().unwrap(), bytes);
}

#[test]
fn transient_hydration_keeps_the_body_remote_and_reclaims_its_spool() {
    let server = LocalServerFixture::new();
    let (_root, mut store) = local();
    let core = server.client(&store);
    let bytes = vec![89; 128 * 1024 + 1];
    let hash = put_asset(&mut store, "assets/transient.png", &bytes)
        .object_hash
        .unwrap();
    let request = super::lww_tests::header(&store);
    core.push(&mut store, &request, &[]).unwrap();
    store
        .asset_residency_set_policy(super::residency::AssetPolicy::Remote, || Ok(()))
        .unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    let cas = PayloadCas::new(store.repository_root()).unwrap();
    let downloaded = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let observed = downloaded.clone();
    let mut body = super::residency::open_transient_server_observed(
        store.repository_root(),
        store.repository_root(),
        &hash,
        &|| Ok(()),
        Some(std::sync::Arc::new(move |bytes| { observed.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed); })),
    )
    .unwrap()
    .unwrap();
    let received = downloaded.load(std::sync::atomic::Ordering::Relaxed);
    assert!(received > 0);
    let mut actual = Vec::new();
    std::io::Read::read_to_end(&mut body, &mut actual).unwrap();
    assert_eq!(actual, bytes);
    assert_eq!(downloaded.load(std::sync::atomic::Ordering::Relaxed), received);
    assert!(cas.stat_object(&hash).unwrap().is_none());
    assert!(Residency::open(store.repository_root()).unwrap().object(&hash,None).unwrap().is_some());
    drop(body);
    assert!(!std::fs::read_dir(store.repository_root())
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("asset-transient-")));
}

#[test]
fn frozen_server_transient_source_survives_access_config_replacement() {
    let server = LocalServerFixture::new();
    let (_root, mut store) = local();
    let core = server.client(&store);
    let bytes = vec![97; 128 * 1024 + 1];
    let hash = put_asset(&mut store, "assets/frozen-source.png", &bytes).object_hash.unwrap();
    let request = super::lww_tests::header(&store);
    core.push(&mut store, &request, &[]).unwrap();
    store.asset_residency_set_policy(super::residency::AssetPolicy::Remote, || Ok(())).unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    let residency = Residency::open(store.repository_root()).unwrap();
    let captured = residency.object(&hash, None).unwrap().unwrap();
    let serialized = serde_json::to_value(&captured).unwrap();
    assert!(serialized["config"].get("token").is_none());
    assert!(serialized["config"].get("directory").is_none());
    assert!(serialized["config"]["credentialId"].is_string());
    let frozen: super::residency::RemoteObject = serde_json::from_value(serialized.clone()).unwrap();

    let other_server = LocalServerFixture::new();
    let mut replacement = captured.config.clone();
    replacement.endpoint = other_server.endpoint.clone();
    residency.replace_access_config(&replacement).unwrap();
    store.server_save_config(&replacement).unwrap();
    assert_eq!(residency.object(&hash, None).unwrap().unwrap().config.endpoint, other_server.endpoint);
    assert_eq!(serde_json::to_value(&frozen).unwrap(), serialized);

    let scratch = tempfile::tempdir().unwrap();
    let mut body = super::residency::open_transient_server_proof_with_check(
        store.repository_root(), scratch.path(), &frozen, &|| Ok(()),
    ).unwrap().unwrap();
    let mut actual = Vec::new();
    std::io::Read::read_to_end(&mut body, &mut actual).unwrap();
    assert_eq!(actual, bytes);
    assert_eq!(body.len().unwrap(), frozen.size);
    assert!(PayloadCas::new(store.repository_root()).unwrap().stat_object(&hash).unwrap().is_none());
    assert_eq!(residency.object(&hash, None).unwrap().unwrap().retention_id, frozen.retention_id);
    drop(body);
    assert!(std::fs::read_dir(scratch.path()).unwrap().next().is_none());
}

#[test]
fn frozen_server_transient_authority_failure_stops_before_body_transfer() {
    let server = LocalServerFixture::new();
    let (_root, mut store) = local();
    let core = server.client(&store);
    let bytes = vec![101; 128 * 1024 + 1];
    let hash = put_asset(&mut store, "assets/frozen-authority.png", &bytes).object_hash.unwrap();
    let request = super::lww_tests::header(&store);
    core.push(&mut store, &request, &[]).unwrap();
    store.asset_residency_set_policy(super::residency::AssetPolicy::Remote, || Ok(())).unwrap();
    store.asset_residency_evict(|| Ok(())).unwrap();
    let proof = Residency::open(store.repository_root()).unwrap().object(&hash, None).unwrap().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let counters = Arc::new(super::client::TestIoCounters::default());
    let error = super::client::with_test_io(store.repository_root(), counters.clone(), || {
        super::residency::open_transient_server_proof_with_check(
            store.repository_root(), scratch.path(), &proof, &|| {
                if counters.snapshot()[0] != 0 { return Err(super::SyncError::new("stale-authority", 409)); }
                Ok(())
            },
        )
    }).err().expect("authority must be checked again after identity resolution");
    assert_eq!(error.code, "stale-authority");
    assert_eq!(counters.snapshot()[0], 1);
    assert!(std::fs::read_dir(scratch.path()).unwrap().next().is_none());
    assert!(PayloadCas::new(store.repository_root()).unwrap().stat_object(&hash).unwrap().is_none());
    assert_eq!(Residency::open(store.repository_root()).unwrap().object(&hash, None).unwrap().unwrap().retention_id, proof.retention_id);
}

#[test]
fn server_only_transient_source_does_not_hydrate_without_server_custody() {
    let (_root,store)=local();
    let scratch=tempfile::tempdir().unwrap();
    let hash="a".repeat(64);
    assert!(super::residency::open_transient_server_with_check(store.repository_root(),scratch.path(),&hash,&||Ok(())).unwrap().is_none());
    assert!(PayloadCas::new(store.repository_root()).unwrap().stat_object(&hash).unwrap().is_none());
    assert!(std::fs::read_dir(scratch.path()).unwrap().next().is_none());
}

#[test]
fn residency_classification_preserves_live_plugin_snapshot_and_durable_job_roots() {
    let (_directory, mut store) = local();
    let root = store.repository_root().to_path_buf();
    let cas = PayloadCas::new(&root).unwrap();
    let live = put_asset(&mut store, "assets/classification.png", b"synthetic live")
        .object_hash
        .unwrap();
    let plugin = cas.prepare_bytes(b"synthetic plugin").unwrap().content_hash;
    store
        .commit(&WorkingSetCommit {
            expected_revision: store.revision().unwrap(),
            plugin_storage: Some(vec![PluginStorageMutation::Set {
                owner: "synthetic".into(),
                key: "payload".into(),
                value: serde_json::json!(crate::asset_repository::object_physical_key(&plugin)),
            }]),
            ..Default::default()
        })
        .unwrap();
    store.snapshot_create("synthetic-classification").unwrap();
    let mut job = DurableCasJob::begin(
        &root,
        "synthetic-classification-job",
        CasJobKind::OfficialPublicationOrExportPreparation,
        crate::asset_repository::job_pins::CasJobOwner::for_test(),
        1,
    )
    .unwrap();
    let pinned = job
        .prepare_bytes(&cas, b"synthetic job-only", CasObjectRole::DirectObject)
        .unwrap()
        .content_hash;
    job.seal(&mut store, 1).unwrap();
    let baseline = store
        .residency_inventory_classification_test(false)
        .unwrap();
    let _guard = crate::asset_repository::coordinator::lock_repository_mutation().unwrap();
    assert_eq!(
        baseline,
        store.residency_inventory_classification_test(true).unwrap()
    );
    assert!(!baseline.2);
    for hash in [&live, &plugin, &pinned] {
        assert!(baseline.0.contains(hash));
    }
    assert!(baseline.1.contains(&pinned));
}

#[test]
fn active_durable_job_waits_for_sealing_before_asset_download() {
    let (_directory, mut store) = local();
    let mut job = DurableCasJob::begin(
        store.repository_root(),
        "synthetic-active-job",
        CasJobKind::OfficialPublicationOrExportPreparation,
        crate::asset_repository::job_pins::CasJobOwner::for_test(),
        1,
    )
    .unwrap();
    for guarded in [false, true] {
        let _guard = guarded
            .then(|| crate::asset_repository::coordinator::lock_repository_mutation().unwrap());
        let error = store.residency_inventory_classification_test(guarded).unwrap_err();
        assert_eq!(
            (error.code.as_str(), error.status, error.retryable),
            ("asset-jobs-busy", 409, true)
        );
    }
    let error = store.asset_residency_download_remote(None, None, None, || Ok(())).err().unwrap();
    assert_eq!((error.code.as_str(), error.retryable), ("asset-jobs-busy", true));
    job.seal(&mut store, 2).unwrap();
    store.asset_residency_download_remote(None, None, None, || Ok(())).unwrap();
}

#[test]
fn abandoned_durable_job_keeps_asset_download_blocked() {
    let (_directory, store) = local();
    drop(DurableCasJob::begin(
        store.repository_root(),
        "synthetic-abandoned-job",
        CasJobKind::OfficialPublicationOrExportPreparation,
        crate::asset_repository::job_pins::CasJobOwner::for_test(),
        1,
    ).unwrap());
    let error = store.asset_residency_download_remote(None, None, None, || Ok(())).err().unwrap();
    assert_eq!(
        (error.code.as_str(), error.status, error.retryable),
        ("asset-jobs-unresolved", 409, false)
    );
}

#[test]
fn corrupt_durable_job_proof_blocks_residency_cleanup_before_any_release() {
    let (_directory, store) = local();
    let root = store.repository_root();
    let job = DurableCasJob::begin(
        root,
        "synthetic-broken-job",
        CasJobKind::OfficialPublicationOrExportPreparation,
        crate::asset_repository::job_pins::CasJobOwner::for_test(),
        1,
    )
    .unwrap();
    drop(job);
    let _active = DurableCasJob::begin(
        root,
        "synthetic-active-job",
        CasJobKind::OfficialPublicationOrExportPreparation,
        crate::asset_repository::job_pins::CasJobOwner::for_test(),
        1,
    ).unwrap();
    std::fs::write(
        root.join("assets/job-pins/job-synthetic-broken-job.journal"),
        b"synthetic corruption",
    )
    .unwrap();
    for guarded in [false, true] {
        let _guard = guarded
            .then(|| crate::asset_repository::coordinator::lock_repository_mutation().unwrap());
        let error = store
            .residency_inventory_classification_test(guarded)
            .unwrap_err();
        assert_eq!(
            (error.code.as_str(), error.status, error.retryable),
            ("asset-jobs-unresolved", 409, false)
        );
    }
}

#[derive(Default)]
struct ReleaseTrace {
    armed: AtomicBool,
    target: Mutex<Option<(std::path::PathBuf, String)>>,
    requests: Mutex<Vec<Vec<String>>>,
}
#[test]
fn a_plugin_root_added_after_first_release_page_keeps_every_later_custody_proof() {
    let trace = Arc::new(ReleaseTrace::default());
    let observed = trace.clone();
    let server = LocalServerFixture::with_router(move |router| {
        router.layer(axum::middleware::from_fn(
            move |request: axum::extract::Request, next: axum::middleware::Next| {
                let trace = observed.clone();
                async move {
                    if request.method() == axum::http::Method::POST
                        && request.uri().path() == "/objects/retention/release"
                    {
                        let (parts, body) = request.into_parts();
                        let bytes =
                            axum::body::to_bytes(body, risunest_sync_wire::MAX_METADATA_BYTES)
                                .await
                                .unwrap();
                        let raw = if parts.headers.contains_key(risunest_sync_wire::body::ENCODING_HEADER) {
                            risunest_sync_wire::body::decode(&bytes, risunest_sync_wire::MAX_METADATA_BYTES).unwrap()
                        } else {
                            bytes.to_vec()
                        };
                        let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
                        let hashes = value["objects"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|item| item["hash"].as_str().unwrap().to_owned())
                            .collect::<Vec<_>>();
                        trace.requests.lock().unwrap().push(hashes.clone());
                        if trace.armed.swap(false, Ordering::SeqCst) {
                            let (root, target) = trace.target.lock().unwrap().clone().unwrap();
                            assert!(!hashes.contains(&target));
                            let mut store = PersistentStore::open(&root).unwrap();
                            store
                                .commit(&WorkingSetCommit {
                                    expected_revision: store.revision().unwrap(),
                                    plugin_storage: Some(vec![PluginStorageMutation::Set {
                                        owner: "synthetic-release".into(),
                                        key: "later-page".into(),
                                        value: serde_json::json!(
                                            crate::asset_repository::object_physical_key(&target)
                                        ),
                                    }]),
                                    ..Default::default()
                                })
                                .unwrap();
                        }
                        return next
                            .run(axum::extract::Request::from_parts(
                                parts,
                                axum::body::Body::from(bytes),
                            ))
                            .await;
                    }
                    next.run(request).await
                }
            },
        ))
    });
    let (_root, store) = local();
    let core = server.client(&store);
    let config = core.client.config();
    let stored = store.server_stored_config().unwrap().unwrap();
    let device = server
        .server
        .authenticate(&config.library_id, &config.token)
        .unwrap();
    let head = server.server.head().unwrap();
    let mut objects = Vec::new();
    for index in 0..300 {
        let body = format!("synthetic release {index}").into_bytes();
        let hash = risunest_sync_wire::hash(&body);
        server.server.put_object(&device, &hash, &body).unwrap();
        objects.push((hash, Some(body.len() as u64)));
    }
    objects.sort_by(|left, right| left.0.cmp(&right.0));
    let target = objects.last().unwrap().0.clone();
    let context = Residency::context_id(&stored, &head.epoch);
    Residency::open(store.repository_root())
        .unwrap()
        .retain(&core.client, &stored, &head, &objects)
        .unwrap();
    *trace.target.lock().unwrap() = Some((store.repository_root().to_path_buf(), target));
    trace.armed.store(true, Ordering::SeqCst);
    let revision = store.revision().unwrap();
    store.asset_residency_release_unused(|| Ok(())).unwrap();
    assert!(!trace.armed.load(Ordering::SeqCst));
    assert_eq!(store.revision().unwrap(), revision + 1);
    let requests = trace.requests.lock().unwrap();
    assert_eq!(requests.iter().map(Vec::len).collect::<Vec<_>>(), vec![128]);
    let residency = Residency::open(store.repository_root()).unwrap();
    let retained = server
        .server
        .retained_objects(&device, &head.epoch, None)
        .unwrap();
    for (hash, _) in &objects[128..] {
        assert!(residency.object(hash, Some(&context)).unwrap().is_some());
        assert!(retained.objects.iter().any(|item| item.hash == *hash));
    }
}
