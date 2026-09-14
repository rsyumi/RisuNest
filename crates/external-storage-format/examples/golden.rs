//! Synthetic interoperability vector generator, excluded from normal builds.
use risunest_external_storage_format::{
    content_identity::hash,
    control::{BackupPointDocument, BackupPointKind, HeadDocument},
    crypto, pack,
    snapshot::{
        combine_content_fingerprint, envelope_length, keyed_object_id, open_envelope,
        seal_envelope, ObjectRole, PublicObjectHeader, SnapshotDocument, StoredObject, WireLocator,
    },
};

const REPOSITORY: &str = "synthetic-repository";
const LIBRARY: &str = "synthetic-library";
const METADATA: &str = "https://synthetic.invalid/folder";
const OBJECT_NAMESPACE: &str = "synthetic-capture-job";
const MAX_DOCUMENT_BYTES: usize = 64 * 1024;

fn bytes(value: &serde_json::Value, key: &str) -> Vec<u8> {
    serde_json::from_value(value[key].clone()).unwrap()
}

fn stored(role: ObjectRole, object_id: &str, plaintext: &[u8]) -> StoredObject {
    let header = PublicObjectHeader::new(
        REPOSITORY.into(),
        object_id.into(),
        role,
        plaintext.len() as u64,
    )
    .unwrap();
    StoredObject {
        ciphertext_length: envelope_length(&header).unwrap(),
        ciphertext_sha256: [2; 32],
        plaintext_length: plaintext.len() as u64,
        plaintext_sha256: hash(plaintext),
        locator: WireLocator {
            connection_identity: "synthetic-account/root".into(),
            collection: Some("synthetic".into()),
            object: format!("opaque-{object_id}"),
        },
        header,
    }
}

fn documents() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let record_catalog = stored(ObjectRole::Catalog, "catalog-records", b"records");
    let asset_catalog = stored(ObjectRole::Catalog, "catalog-assets", b"assets");
    let scope_id = [4; 32];
    let library_fingerprint = [5; 32];
    let fingerprint = combine_content_fingerprint(&scope_id, &library_fingerprint, None);
    let snapshot = SnapshotDocument::new(
        "snapshot-synthetic".into(),
        REPOSITORY.into(),
        LIBRARY.into(),
        "synthetic-device".into(),
        1_726_272_000_000,
        17,
        scope_id,
        scope_id,
        Some("snapshot-parent".into()),
        fingerprint,
        library_fingerprint,
        record_catalog,
        asset_catalog,
        None,
        None,
        Vec::new(),
    )
    .unwrap();
    let snapshot_bytes = snapshot.encode(MAX_DOCUMENT_BYTES).unwrap();
    let snapshot_object = stored(ObjectRole::Snapshot, "snapshot-synthetic", &snapshot_bytes);
    let head = HeadDocument::new(
        REPOSITORY.into(),
        LIBRARY.into(),
        "commit-synthetic".into(),
        Some("commit-parent".into()),
        scope_id,
        fingerprint,
        snapshot_object.clone(),
    )
    .unwrap()
    .encode(MAX_DOCUMENT_BYTES)
    .unwrap();
    let point = BackupPointDocument::new(
        REPOSITORY.into(),
        "point-synthetic".into(),
        BackupPointKind::Manual,
        1_726_272_000_001,
        17,
        scope_id,
        vec![snapshot_object],
    )
    .unwrap()
    .encode(MAX_DOCUMENT_BYTES)
    .unwrap();
    (snapshot_bytes, head, point)
}

fn seal(bytes: &[u8], key: &[u8; 32], object_id: &str, role: ObjectRole) -> Vec<u8> {
    let header = PublicObjectHeader::new(
        REPOSITORY.into(),
        object_id.into(),
        role,
        bytes.len() as u64,
    )
    .unwrap();
    let mut envelope = Vec::new();
    seal_envelope(
        &mut std::io::Cursor::new(bytes),
        &mut envelope,
        key,
        &header,
    )
    .unwrap();
    envelope
}

fn open(envelope: &[u8], key: &[u8; 32], object_id: &str, role: ObjectRole) -> Vec<u8> {
    let mut plaintext = Vec::new();
    let header = open_envelope(
        &mut std::io::Cursor::new(envelope),
        &mut plaintext,
        key,
        MAX_DOCUMENT_BYTES as u64,
    )
    .unwrap();
    assert_eq!(header.repository_id, REPOSITORY);
    assert_eq!(header.object_id, object_id);
    assert_eq!(header.role, role);
    assert_eq!(header.plaintext_length, plaintext.len() as u64);
    plaintext
}

fn verify_wasm_vector(value: serde_json::Value) {
    let plaintext = bytes(&value, "plaintext");
    assert_eq!(
        pack::decompress(
            &bytes(&value, "compressed"),
            plaintext.len(),
            &hash(&plaintext),
        )
        .unwrap(),
        plaintext
    );
    let code = crypto::RecoveryCode::parse(value["code"].as_str().unwrap()).unwrap();
    let recovery_bytes = bytes(&value, "recovery");
    let recovery = crypto::RecoveryEnvelope::decode(&recovery_bytes).unwrap();
    assert_eq!(recovery.encode().unwrap(), recovery_bytes);
    let recovered = recovery.recover(REPOSITORY, &code).unwrap();
    assert_eq!(*recovered.root, [7; 32]);
    assert_eq!(&*recovered.connection_metadata, METADATA);

    let (snapshot, head, point) = documents();
    let key = [7; 32];
    let opened_snapshot = open(
        &bytes(&value, "snapshotEnvelope"),
        &key,
        "snapshot-synthetic",
        ObjectRole::Snapshot,
    );
    assert_eq!(opened_snapshot, snapshot);
    SnapshotDocument::decode(&opened_snapshot, MAX_DOCUMENT_BYTES).unwrap();
    let opened_head = open(
        &bytes(&value, "headEnvelope"),
        &key,
        "head",
        ObjectRole::Head,
    );
    assert_eq!(opened_head, head);
    HeadDocument::decode(&opened_head, MAX_DOCUMENT_BYTES).unwrap();
    let opened_point = open(
        &bytes(&value, "pointEnvelope"),
        &key,
        "point-synthetic",
        ObjectRole::BackupPoint,
    );
    assert_eq!(opened_point, point);
    BackupPointDocument::decode(&opened_point, MAX_DOCUMENT_BYTES).unwrap();

    let document_hash = hash(&snapshot);
    assert_eq!(
        value["keyedPackId"].as_str().unwrap(),
        keyed_object_id(&key, OBJECT_NAMESPACE, ObjectRole::Pack, &document_hash).unwrap()
    );
    assert_eq!(
        value["keyedCatalogId"].as_str().unwrap(),
        keyed_object_id(&key, OBJECT_NAMESPACE, ObjectRole::Catalog, &document_hash).unwrap()
    );
    println!("WASM-to-native RNX1, control, naming and recovery vectors passed.");
}

fn main() {
    if let Some(path) = std::env::args().nth(1) {
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        verify_wasm_vector(value);
        return;
    }

    let plaintext = (0..100_005)
        .map(|index| (index % 251) as u8)
        .collect::<Vec<_>>();
    let key = [7; 32];
    let binding = "synthetic-repository/synthetic-object/data/v1";
    let mut ciphertext = Vec::new();
    let code = crypto::RecoveryCode::generate().unwrap();
    let recovery =
        crypto::RecoveryEnvelope::protect(REPOSITORY.into(), METADATA.into(), &key, &code)
            .unwrap()
            .encode()
            .unwrap();
    let compressed = pack::compress(&plaintext).unwrap();
    crypto::encrypt(
        &mut std::io::Cursor::new(&plaintext),
        &mut ciphertext,
        &key,
        binding.as_bytes(),
        plaintext.len() as u64,
    )
    .unwrap();
    let (snapshot, head, point) = documents();
    let document_hash = hash(&snapshot);
    println!(
        "{}",
        serde_json::json!({
            "key": key,
            "binding": binding,
            "plaintext": plaintext,
            "ciphertext": ciphertext,
            "hash": hash(&plaintext),
            "compressed": compressed,
            "code": &*code.expose(),
            "recovery": recovery,
            "snapshot": snapshot,
            "head": head,
            "point": point,
            "snapshotEnvelope": seal(&snapshot, &key, "snapshot-synthetic", ObjectRole::Snapshot),
            "headEnvelope": seal(&head, &key, "head", ObjectRole::Head),
            "pointEnvelope": seal(&point, &key, "point-synthetic", ObjectRole::BackupPoint),
            "objectNamespace": OBJECT_NAMESPACE,
            "keyedPackId": keyed_object_id(&key, OBJECT_NAMESPACE, ObjectRole::Pack, &document_hash).unwrap(),
            "keyedCatalogId": keyed_object_id(&key, OBJECT_NAMESPACE, ObjectRole::Catalog, &document_hash).unwrap(),
        })
    );
}
