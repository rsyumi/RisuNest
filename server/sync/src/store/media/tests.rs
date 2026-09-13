use super::*;
use risunest_sync_connect::media::{MediaObject, MediaSigner};

fn fixture() -> (tempfile::TempDir, Store, Device, MediaRequest) {
    let root = tempfile::tempdir().unwrap();
    let store = Store::init(root.path()).unwrap();
    let device = Device {
        id: store.add_device().unwrap().device_id,
    };
    let bytes = b"synthetic media bytes";
    let hash = risunest_sync_wire::hash(bytes);
    store.put_object(&device, &hash, bytes).unwrap();
    let object = MediaObject {
        hash,
        size: (bytes.len() as u64).into(),
        mime: "image/png".into(),
    };
    let signer = MediaSigner::new(&[4; 32]).unwrap();
    let request = MediaRequest {
        refresh_url: signer
            .refresh_url("http://127.0.0.1:12345", &object)
            .unwrap(),
        object,
    };
    (root, store, device, request)
}

#[test]
fn signed_access_survives_server_restart_and_is_revoked_with_device() {
    let (root, store, device, request) = fixture();
    let epoch = store.head().unwrap().epoch;
    let access = store
        .media_access(&device, &epoch, &[request.clone()])
        .unwrap()
        .remove(0);
    assert!(matches!(
        store.resolve_media(&access.token).unwrap(),
        MediaResponse::File { size: 21, .. }
    ));
    drop(store);
    let store = Store::open(root.path()).unwrap();
    assert!(matches!(
        store.resolve_media(&access.token).unwrap(),
        MediaResponse::File { .. }
    ));
    store.revoke_device(&device.id).unwrap();
    assert!(store.resolve_media(&access.token).is_err());
}

#[test]
fn expiration_returns_only_a_scoped_refresh_and_forgery_never_redirects() {
    let (_root, store, device, request) = fixture();
    let head = store.head().unwrap();
    let claims = MediaClaims {
        library_id: head.library_id,
        epoch: head.epoch,
        device_id: device.id,
        request: request.clone(),
        expires: 0.into(),
    };
    let token = store.media_signer.sign_access(&claims).unwrap();
    assert!(
        matches!(store.resolve_media(&token).unwrap(),MediaResponse::Refresh(url) if url==request.refresh_url)
    );
    let fake = MediaSigner::new(&[99; 32])
        .unwrap()
        .sign_access(&claims)
        .unwrap();
    assert!(store.resolve_media(&fake).is_err());
    store.rotate_restored_epoch().unwrap();
    assert!(store.resolve_media(&token).is_err());
}

#[test]
fn size_epoch_and_callback_scope_are_validated_before_grant() {
    let (_root, store, device, request) = fixture();
    let epoch = store.head().unwrap().epoch;
    assert!(store
        .media_access(&device, "wrong-epoch", &[request.clone()])
        .is_err());
    let mut altered = request.clone();
    altered.object.size = 1.into();
    altered.refresh_url = MediaSigner::new(&[4; 32])
        .unwrap()
        .refresh_url("http://127.0.0.1:12345", &altered.object)
        .unwrap();
    assert!(store.media_access(&device, &epoch, &[altered]).is_err());
    let mut altered = request;
    altered.refresh_url = "http://127.0.0.1:12345/admin".into();
    assert!(store.media_access(&device, &epoch, &[altered]).is_err());
}
