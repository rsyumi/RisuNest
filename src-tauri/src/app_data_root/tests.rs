use super::resolve_platform_root;
use crate::asset_repository::{ExactObjectUnlink, PayloadCas};
use std::{fs, io::ErrorKind, path::Path};

#[test]
fn linux_data_home_uses_only_absolute_xdg_or_home_paths() {
    let root = tempfile::tempdir().unwrap();
    let xdg = root.path().join("xdg");
    assert_eq!(
        super::linux_data_home(Some(xdg.clone()), None).unwrap(),
        xdg
    );
    assert_eq!(
        super::linux_data_home(Some("relative".into()), Some(root.path().to_owned())).unwrap(),
        root.path().join(".local/share")
    );
    assert!(super::linux_data_home(None, Some("relative".into())).is_err());
}

#[test]
fn windows_data_roots_use_dedicated_pascal_case_leaf_names() {
    let base = tempfile::tempdir().unwrap().path().to_owned();
    assert_eq!(
        super::windows_data_root(base.clone(), "RisuNestData").unwrap(),
        base.join("RisuNestData")
    );
    assert_eq!(
        super::windows_data_root(base.clone(), "RisuNestWebViewData").unwrap(),
        base.join("RisuNestWebViewData")
    );
    assert!(super::windows_data_root("relative".into(), "RisuNestData").is_err());
}

#[cfg(unix)]
fn link_directory(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).expect("create synthetic directory link");
}

#[cfg(windows)]
fn link_directory(target: &Path, link: &Path) {
    let output = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .output()
        .expect("create synthetic directory junction");
    assert!(output.status.success(), "create junction: {output:?}");
}

#[test]
fn platform_parent_alias_supports_cas_write_read_and_delete() {
    let fixture = tempfile::tempdir().unwrap();
    let real_parent = fixture.path().join("data");
    let real_root = real_parent.join("synthetic.app");
    fs::create_dir_all(&real_root).unwrap();
    let alias_parent = fixture.path().join("user-0");
    link_directory(&real_parent, &alias_parent);
    let platform_root = alias_parent.join("synthetic.app");

    // Untrusted CAS inputs must still reject links in their ancestry.
    let error = PayloadCas::new(&platform_root).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidData);
    assert!(error
        .to_string()
        .starts_with("linked path component is forbidden:"));

    let root = resolve_platform_root(&platform_root, false).unwrap();
    assert_eq!(root, resolve_platform_root(&platform_root, true).unwrap());
    assert_eq!(root, fs::canonicalize(&real_root).unwrap());
    let cas = PayloadCas::new(&root).expect("open CAS through resolved platform root");
    let payload = cas.prepare_bytes(b"synthetic Android asset").unwrap();
    assert_eq!(
        cas.read_object(&payload.content_hash).unwrap().unwrap(),
        b"synthetic Android asset"
    );
    assert!(matches!(
        cas.unlink_exact_object(
            &payload.content_hash,
            payload.byte_size,
            &payload.physical_key
        )
        .unwrap(),
        ExactObjectUnlink::Removed { .. }
    ));
    assert!(cas.read_object(&payload.content_hash).unwrap().is_none());
}

#[test]
fn platform_root_does_not_assume_primary_user_or_package_name() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path().join("user/10/another.synthetic.app");
    fs::create_dir_all(&root).unwrap();
    assert_eq!(
        resolve_platform_root(&root, false).unwrap(),
        fs::canonicalize(root).unwrap()
    );
}

#[test]
fn platform_root_itself_cannot_be_a_link() {
    let fixture = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = fixture.path().join("linked-app");
    link_directory(outside.path(), &root);
    assert_eq!(
        resolve_platform_root(&root, false).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
}

#[test]
fn platform_root_must_be_an_existing_absolute_directory() {
    let fixture = tempfile::tempdir().unwrap();
    let file = fixture.path().join("file");
    fs::write(&file, b"synthetic").unwrap();
    assert_eq!(
        resolve_platform_root(&file, false).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(
        resolve_platform_root(&fixture.path().join("missing"), false)
            .unwrap_err()
            .kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        resolve_platform_root(Path::new("relative-app"), false)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
}

#[test]
fn first_run_creates_a_real_root_beneath_a_trusted_parent_alias() {
    let fixture = tempfile::tempdir().unwrap();
    let parent = fixture.path().join("real-parent");
    fs::create_dir(&parent).unwrap();
    let alias = fixture.path().join("home-alias");
    link_directory(&parent, &alias);
    let root = resolve_platform_root(&alias.join("RisuNest"), true).unwrap();
    assert_eq!(root, parent.join("RisuNest").canonicalize().unwrap());
    let cas = PayloadCas::new(&root).unwrap();
    let payload = cas.prepare_bytes(b"first-run asset").unwrap();
    assert_eq!(cas.read_object(&payload.content_hash).unwrap().unwrap(), b"first-run asset");
}

#[test]
fn first_run_does_not_accept_a_linked_or_nonabsolute_root() {
    let fixture = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = fixture.path().join("linked-app");
    link_directory(outside.path(), &root);
    assert_eq!(resolve_platform_root(&root, true).unwrap_err().kind(), ErrorKind::InvalidData);
    assert_eq!(resolve_platform_root(Path::new("relative-app"), true).unwrap_err().kind(), ErrorKind::InvalidInput);
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn resolved_root_does_not_allow_asset_directory_redirection() {
    let fixture = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = resolve_platform_root(fixture.path(), false).unwrap();
    let cas = PayloadCas::new(&root).unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, b"untouched").unwrap();
    link_directory(outside.path(), &root.join("assets-v2"));
    assert_eq!(
        cas.prepare_bytes(b"new asset").unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(
        cas.read_object(&"0".repeat(64)).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(fs::read(sentinel).unwrap(), b"untouched");
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 1);
}

#[test]
fn resolved_root_replaced_by_link_is_rejected_by_cas() {
    let fixture = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let platform_root = fixture.path().join("app");
    fs::create_dir(&platform_root).unwrap();
    let root = resolve_platform_root(&platform_root, false).unwrap();
    let cas = PayloadCas::new(&root).unwrap();
    fs::remove_dir(&platform_root).unwrap();
    link_directory(outside.path(), &platform_root);
    assert_eq!(
        cas.prepare_bytes(b"new asset").unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert!(PayloadCas::new(&root).is_err());
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
}
