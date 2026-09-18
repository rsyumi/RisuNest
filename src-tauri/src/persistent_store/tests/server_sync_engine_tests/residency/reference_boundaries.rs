use super::*;
use crate::server_sync::backups::Side;
use risunest_sync_wire::{CommitIntent, RecordChange, RecordVersion, RemoteHead};

fn commit_records(
    server: &Store,
    device: &risunest_sync_server::store::Device,
    sequence: u64,
    changes: Vec<RecordChange>,
) -> RemoteHead {
    let expected = server.head().unwrap();
    let staged = server.stage_changes(device, &ChangeSet {
        changes, read_fences: vec![], scope_fences: vec![],
    }).unwrap();
    let receipt = server.commit(device, &CommitIntent {
        device_operation_seq: sequence.into(),
        expected_head: expected.clone(),
        changes_digest: staged.changes_digest,
        staged_changes_id: staged.staged_changes_id,
    }, &expected.etag()).unwrap();
    assert_eq!(receipt.head.seq, expected.seq.next().unwrap());
    receipt.head
}

#[test]
fn checkpoint_pages_keep_the_original_head_when_a_later_commit_changes_unread_records() {
    let (fixture, trace) = measured_fixture();
    let (_root, mut local) = prepared();
    fixture.bind(&mut local);
    let config = local.server_config().unwrap().unwrap();
    let device = fixture.server.authenticate(&config.library_id, &config.token).unwrap();
    let mut keys = (0..references::PAGE * 2 + 3).map(|number|
        crate::logical_records::encode_logical_record_key(&crate::logical_records::LogicalRecordLocator::Asset {
            logical_key: format!("assets/checkpoint-{number:04}.png"),
        }).unwrap()).collect::<Vec<_>>();
    keys.sort();
    let before = RecordVersion::Tombstone { deletion_id: "original-deletion".into() };
    let expected = commit_records(&fixture.server, &device, 1, keys.iter().map(|key| RecordChange {
        domain: Domain::Library, key: key.clone(), before: RecordVersion::Absent, after: before.clone(),
    }).collect());
    let client = ServerClient::new(config).unwrap();
    trace.armed.store(true, Ordering::SeqCst);
    let read = references::RemoteRead::begin(&client, &expected).unwrap();
    let mut visited = Vec::new();
    read.visit(|record| {
        if visited.is_empty() {
            commit_records(&fixture.server, &device, 2, vec![RecordChange {
                domain: Domain::Library, key: keys.last().unwrap().clone(), before: before.clone(),
                after: RecordVersion::Tombstone { deletion_id: "later-deletion".into() },
            }]);
        }
        assert_eq!(record.version, before, "a checkpoint must not follow the live head between pages");
        visited.push(record.key);
        Ok(())
    }).unwrap();
    assert_eq!(visited, keys);
    assert_ne!(fixture.server.head().unwrap(), expected);
    let requests = trace.requests.lock().unwrap();
    let pages = requests.iter().filter(|request| request.method == "GET").collect::<Vec<_>>();
    assert_eq!(pages.len(), 3);
    assert!(pages.iter().all(|request| request.path.starts_with("/checkpoints/")
        && request.query.as_ref().unwrap().split('&').any(|part| part == "limit=256")));
    assert_eq!(requests.iter().filter(|request| request.method == "POST" && request.path == "/checkpoints").count(), 1);
    drop(requests);
    trace.armed.store(false, Ordering::SeqCst);
    read.release().unwrap();
}

#[test]
fn partial_retention_and_failed_confirmation_keep_roots_and_resume_the_same_capture() {
    let (fixture, trace) = measured_fixture();
    let (root, mut local) = prepared();
    fixture.bind(&mut local);
    let config = local.server_config().unwrap().unwrap();
    let device = fixture.server.authenticate(&config.library_id, &config.token).unwrap();
    let stored = local.server_stored_config().unwrap().unwrap();
    let head = fixture.server.head().unwrap();
    let revision = local.revision().unwrap();
    let generation = active_generation(&local.connection).unwrap();
    let context = Residency::context_id(&stored, &head.epoch);
    let mut capture = references::Capture::begin(root.path(), revision, &generation, &head).unwrap();
    let metadata = capture.metadata(Side::Local, b"synthetic pinned metadata").unwrap();
    let id = capture.id.clone();
    let mut hashes = Vec::new();
    for number in 0..references::PAGE + 1 {
        let bytes = format!("synthetic remote payload {number}").into_bytes();
        let hash = risunest_sync_wire::hash(&bytes);
        fixture.server.put_object(&device, &hash, &bytes).unwrap();
        capture.object(Side::Remote, &references::Object {
            hash: hash.clone(), byte_size: Some(bytes.len() as u64), metadata: false,
            context_id: Some(context.clone()), local_required: false,
        }).unwrap();
        hashes.push(hash);
    }
    hashes.sort();
    capture.complete_side(Side::Local).unwrap();
    capture.complete_side(Side::Remote).unwrap();
    *trace.root.lock().unwrap() = Some(root.path().to_path_buf());
    trace.armed.store(true, Ordering::SeqCst);
    trace.fail_retention_page.store(2, Ordering::SeqCst);
    let client = ServerClient::new(config).unwrap();
    let mut residency = Residency::open(root.path()).unwrap();
    assert!(capture.retain(&client, &stored, &mut residency).is_err());
    drop(capture);
    drop(residency);

    let assert_incomplete = || {
        assert!(references::inspect(root.path(), &id).is_err());
        assert!(!DurableCasJob::open(root.path(), &id).unwrap().is_released());
        let residency = Residency::open(root.path()).unwrap();
        let confirmed = hashes.iter().filter(|hash| residency.object(hash, Some(&context)).unwrap().is_some()).count();
        assert_eq!(confirmed, references::PAGE);
        let mut roots = std::collections::BTreeSet::new();
        references::visit_roots(root.path(), |object| { roots.insert(object.hash); Ok(()) }).unwrap();
        assert!(roots.contains(&metadata));
        assert!(hashes.iter().all(|hash| roots.contains(hash)));
        assert_eq!(fixture.server.head().unwrap(), head);
    };
    assert_incomplete();

    // Fail the local confirmation only after the server has granted the last page.
    let db = rusqlite::Connection::open(Residency::path(root.path())).unwrap();
    db.execute_batch(&format!("CREATE TRIGGER fail_reference_confirmation BEFORE INSERT ON objects
        WHEN NEW.hash='{}' BEGIN SELECT RAISE(ABORT,'synthetic confirmation failure'); END;",
        hashes.last().unwrap())).unwrap();
    trace.fail_retention_page.store(0, Ordering::SeqCst);
    let resumed = references::Capture::resume(root.path(), &id, revision, &generation, &head).unwrap();
    let mut residency = Residency::open(root.path()).unwrap();
    assert!(resumed.retain(&client, &stored, &mut residency).is_err());
    drop(resumed);
    drop(residency);
    assert_incomplete();
    db.execute_batch("DROP TRIGGER fail_reference_confirmation").unwrap();
    drop(db);

    let resumed = references::Capture::resume(root.path(), &id, revision, &generation, &head).unwrap();
    let mut residency = Residency::open(root.path()).unwrap();
    resumed.retain(&client, &stored, &mut residency).unwrap();
    let receipt = resumed.finish(&mut local, &|| client.ensure_active()).unwrap();
    assert_eq!(receipt.id, id);
    assert_eq!(local.revision().unwrap(), revision);
    assert_eq!(fixture.server.head().unwrap(), head);
    let requests = trace.requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    for request in requests.iter() {
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/objects/retention");
        assert!(request.body["objects"].as_array().unwrap().len() <= references::PAGE);
    }
    assert!(hashes.iter().all(|hash| PayloadCas::new(root.path()).unwrap().stat_object(hash).unwrap().is_none()));
    assert!(!root.path().join("server-sync/backups").join(&id).join("local.risunest").exists());
    assert!(!root.path().join("server-sync/backups").join(&id).join("remote.risunest").exists());
}
