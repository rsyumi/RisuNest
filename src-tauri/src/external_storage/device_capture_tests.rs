use super::*;

struct Never;
impl CancellationProbe for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
}

fn source_catalog(path: &Path, binary: &[u8]) -> (Connection, String) {
    let db = create_catalog(path).unwrap();
    let metadata =
        r#"{"present":true,"profile":"risunest.device-section/v1","sectionId":"local-storage"}"#;
    let row = format!(
        r#"{{"kind":"value","key":"0073006100660065005f0070006c007500670069006e005f006b00650079","value":{{"version":1,"root":0,"nodes":[{{"type":"blob","reference":"{}","byteLength":{}}}]}}}}"#,
        hex::encode(risunest_external_storage_format::content_identity::hash(
            binary
        )),
        binary.len()
    );
    let mut digest = Sha256::new();
    digest.update(b"RisuNest-device-section-v1\0");
    digest.update((metadata.len() as u64).to_le_bytes());
    digest.update(metadata.as_bytes());
    digest.update(0u64.to_le_bytes());
    digest.update((row.len() as u64).to_le_bytes());
    digest.update(row.as_bytes());
    let section_hash = hex::encode(digest.finalize());
    db.execute(
        "INSERT INTO device_sections VALUES('local-storage',1,1,1,1,1,?1)",
        [&section_hash],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_records VALUES('local-storage',-1,?1)",
        [metadata],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_records VALUES('local-storage',0,?1)",
        [&row],
    )
    .unwrap();
    db.execute(
        "INSERT INTO device_objects VALUES(?1,?2)",
        params![
            hex::encode(risunest_external_storage_format::content_identity::hash(
                binary
            )),
            binary.len() as i64
        ],
    )
    .unwrap();
    (db, section_hash)
}

#[test]
fn logical_device_identity_is_independent_of_library_revision_and_destination() {
    let root = tempfile::tempdir().unwrap();
    let binary = b"synthetic device binary";
    let hash = risunest_external_storage_format::content_identity::hash(binary);
    let path_a = root.path().join("a.sqlite");
    let path_b = root.path().join("b.sqlite");
    let (db_a, _) = source_catalog(&path_a, binary);
    let (db_b, _) = source_catalog(&path_b, binary);
    let file = DeviceFile {
        section_id: format!("device/object/{}", hex::encode(hash)),
        content_hash: hash,
        byte_length: binary.len() as u64,
        path: root.path().join(hex::encode(hash)),
    };
    assert_eq!(
        logical_identity(&db_a, std::slice::from_ref(&file)).unwrap(),
        logical_identity(&db_b, std::slice::from_ref(&file)).unwrap()
    );
}

#[test]
fn reopening_requires_every_referenced_binary_with_exact_bytes() {
    let root = tempfile::tempdir().unwrap();
    checked_directory(root.path()).unwrap();
    checked_directory(&root.path().join("captures")).unwrap();
    checked_directory(&root.path().join("objects")).unwrap();
    let binary = b"synthetic device binary";
    let hash = risunest_external_storage_format::content_identity::hash(binary);
    let staging = root.path().join("staging.sqlite");
    let (db, _) = source_catalog(&staging, binary);
    let blob = DeviceFile {
        section_id: format!("device/object/{}", hex::encode(hash)),
        content_hash: hash,
        byte_length: binary.len() as u64,
        path: root.path().join("objects").join(hex::encode(hash)),
    };
    let (identity, _) = logical_identity(&db, std::slice::from_ref(&blob)).unwrap();
    drop(db);
    let capture_id = hex::encode(identity);
    let directory = root.path().join("captures").join(&capture_id);
    fs::create_dir(&directory).unwrap();
    fs::rename(&staging, directory.join("device.sqlite")).unwrap();
    fs::write(&blob.path, binary).unwrap();
    let snapshot = reopen_device_snapshot(root.path(), &capture_id).unwrap();
    assert_eq!(snapshot.capture_id, capture_id);
    assert_eq!(snapshot.sections, vec!["local-storage"]);
    assert_eq!(snapshot.blobs.len(), 1);

    fs::write(&blob.path, b"changed device binary").unwrap();
    assert!(reopen_device_snapshot(root.path(), &capture_id).is_err());
}

#[test]
fn snapshot_inventory_comes_only_from_native_device_objects() {
    let root = tempfile::tempdir().unwrap();
    let binary = b"synthetic device binary";
    let (db, _) = source_catalog(&root.path().join("capture.sqlite"), binary);
    let listed: Vec<(String, i64)> = db
        .prepare("SELECT sha256,byte_length FROM device_objects")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].1, binary.len() as i64);
}

#[test]
fn downloaded_snapshot_paths_are_reverified_without_local_capture_layout() {
    let root = tempfile::tempdir().unwrap();
    let binary = b"synthetic downloaded device binary";
    let hash = risunest_external_storage_format::content_identity::hash(binary);
    let catalog_path = root.path().join("remote-catalog.sqlite");
    let (db, _) = source_catalog(&catalog_path, binary);
    let blob = DeviceFile {
        section_id: format!("device/object/{}", hex::encode(hash)),
        content_hash: hash,
        byte_length: binary.len() as u64,
        path: root.path().join("opaque-downloaded-object"),
    };
    fs::write(&blob.path, binary).unwrap();
    let (device_identity, sections) = logical_identity(&db, std::slice::from_ref(&blob)).unwrap();
    drop(db);
    let snapshot = DeviceSnapshot {
        capture_id: hex::encode(device_identity),
        device_identity,
        sqlite: catalog_file(catalog_path).unwrap(),
        blobs: vec![blob],
        sections,
    };
    assert!(verify_snapshot(snapshot.clone()).is_ok());

    let mut wrong_sections = snapshot;
    wrong_sections.sections = vec!["device-settings".into()];
    assert!(verify_snapshot(wrong_sections).is_err());
}
