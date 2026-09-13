use super::*;
use serde_json::json;

fn backup(root: &Path, index: u128, complete: bool) -> (String, u64) {
    let id = uuid::Uuid::from_u128(index).to_string();
    let directory = root.join("server-sync/backups").join(&id);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("local.risunest"), b"synthetic local").unwrap();
    fs::write(directory.join("remote.risunest"), b"synthetic remote").unwrap();
    if complete {
        fs::write(directory.join("complete.json"), serde_json::to_vec(&json!({
            "format": "risunest-portable-backup", "scope": "library",
            "head": { "libraryId":"library", "epoch":"epoch", "seq":"0", "headId":"a".repeat(64), "minRetainedSeq":"0" },
            "localRevision": 1,
            "localHash": risunest_sync_wire::hash(b"synthetic local"),
            "remoteHash": risunest_sync_wire::hash(b"synthetic remote"),
        })).unwrap()).unwrap();
    }
    (id, tree_bytes(&directory).unwrap())
}

#[test]
fn inventory_counts_all_files_and_pages_beyond_one_hundred() {
    let root = tempfile::tempdir().unwrap();
    let mut disk_bytes = 0;
    for index in 1..=105 {
        disk_bytes += backup(root.path(), index, true).1;
    }
    let incomplete = backup(root.path(), 106, false).1;
    disk_bytes += incomplete;
    let first = inventory(root.path(), None, None, &BTreeSet::new()).unwrap();
    assert_eq!(first.items.len(), 100);
    assert_eq!(first.complete_count, 105);
    assert_eq!(first.complete_bytes, 105 * (15 + 16));
    assert_eq!(first.incomplete_count, 1);
    assert_eq!(first.incomplete_bytes, incomplete);
    assert_eq!(first.disk_bytes, disk_bytes);
    assert!(first
        .items
        .iter()
        .all(|item| item.deletable && item.backup.preservation_scope == "library"));
    let second = inventory(root.path(), first.next.as_ref(), None, &BTreeSet::new()).unwrap();
    assert_eq!(second.items.len(), 5);
    assert!(second.next.is_none());
    assert_eq!(second.disk_bytes, disk_bytes);
    let ids: BTreeSet<_> = first
        .items
        .iter()
        .chain(&second.items)
        .map(|item| &item.backup.id)
        .collect();
    assert_eq!(ids.len(), 105);
}

#[test]
fn empty_inventory_and_cache_do_not_create_directories() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        inventory(root.path(), None, None, &BTreeSet::new())
            .unwrap()
            .disk_bytes,
        0
    );
    assert_eq!(
        cache_usage(root.path(), None, &BTreeSet::new(), None, false)
            .unwrap()
            .total_bytes,
        0
    );
    assert!(!root.path().join("server-sync").exists());
}

#[test]
fn fresh_ownership_recheck_blocks_listed_backup_and_preserves_incomplete_data() {
    let root = tempfile::tempdir().unwrap();
    let (id, size) = backup(root.path(), 1, true);
    let (unfinished, unfinished_size) = backup(root.path(), 2, false);
    assert!(
        inventory(root.path(), None, None, &BTreeSet::new())
            .unwrap()
            .items[0]
            .deletable
    );
    let pinned = BTreeSet::from([id.clone()]);
    assert_eq!(
        delete_backup(root.path(), &id, None, &pinned)
            .unwrap_err()
            .code,
        "backup-in-use"
    );
    assert_eq!(
        delete_backup(root.path(), &id, Some("server-sync-busy"), &BTreeSet::new())
            .unwrap_err()
            .code,
        "server-sync-busy"
    );
    assert!(delete_backup(root.path(), &unfinished, None, &BTreeSet::new()).is_err());
    assert!(delete_backup(root.path(), "../outside", None, &BTreeSet::new()).is_err());
    assert_eq!(
        inventory(root.path(), None, None, &pinned)
            .unwrap()
            .disk_bytes,
        size + unfinished_size
    );
    delete_backup(root.path(), &id, None, &BTreeSet::new()).unwrap();
    let remaining = inventory(root.path(), None, None, &BTreeSet::new()).unwrap();
    assert_eq!(remaining.complete_count, 0);
    assert_eq!(remaining.disk_bytes, unfinished_size);
}

#[test]
fn interrupted_explicit_deletion_resumes_only_its_id() {
    let root = tempfile::tempdir().unwrap();
    let (id, _) = backup(root.path(), 1, true);
    let (other, other_size) = backup(root.path(), 2, true);
    let base = root.path().join("server-sync/backups");
    let deleting = base.join(format!(".deleting-{id}"));
    fs::rename(base.join(&id), &deleting).unwrap();
    fs::remove_file(deleting.join("complete.json")).unwrap();
    assert_eq!(
        inventory(root.path(), None, None, &BTreeSet::new())
            .unwrap()
            .incomplete_count,
        1
    );
    delete_backup(root.path(), &id, None, &BTreeSet::new()).unwrap();
    assert_eq!(tree_bytes(&base.join(other)).unwrap(), other_size);
    assert!(!deleting.exists());
}

#[test]
fn cache_cleanup_keeps_referenced_objects_metadata_and_local_cas() {
    let root = tempfile::tempdir().unwrap();
    let identity = "a".repeat(64);
    let cache =
        super::super::cache::Cache::open(&root.path().join("server-sync").join(&identity)).unwrap();
    let protected = cache.put(b"referenced base").unwrap();
    let unused = cache.put(b"regenerable data").unwrap();
    fs::write(
        cache.cas.repository_root().join("transfers.sqlite"),
        b"protected metadata",
    )
    .unwrap();
    let live = crate::asset_repository::PayloadCas::new(root.path()).unwrap();
    let local = live.prepare_bytes(b"unsent local source").unwrap();
    let hashes = BTreeSet::from([protected.clone()]);
    let usage = cache_usage(root.path(), Some(&identity), &hashes, None, false).unwrap();
    assert_eq!(usage.reclaimable_bytes, 16);
    assert_eq!(
        usage.total_bytes,
        usage.reclaimable_bytes + usage.protected_bytes
    );
    assert!(cache_usage(
        root.path(),
        Some(&identity),
        &hashes,
        Some("resolve-pending-operation-first"),
        true
    )
    .is_err());
    assert!(cache.cas.open_object(&unused).unwrap().is_some());
    let cleaned = cache_usage(root.path(), Some(&identity), &hashes, None, true).unwrap();
    assert_eq!(cleaned.reclaimable_bytes, 0);
    assert!(cache.cas.open_object(&unused).unwrap().is_none());
    assert!(cache.cas.open_object(&protected).unwrap().is_some());
    assert!(live.open_object(&local.content_hash).unwrap().is_some());
    assert!(cache
        .cas
        .repository_root()
        .join("transfers.sqlite")
        .exists());
}

#[test]
fn linked_backup_and_cache_paths_are_rejected_without_following_them() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("keep"), b"outside").unwrap();
    fs::create_dir_all(root.path().join("server-sync/backups")).unwrap();
    let id = uuid::Uuid::from_u128(1).to_string();
    directory_link(
        outside.path(),
        &root.path().join("server-sync/backups").join(&id),
    );
    assert!(inventory(root.path(), None, None, &BTreeSet::new()).is_err());
    assert!(delete_backup(root.path(), &id, None, &BTreeSet::new()).is_err());
    assert_eq!(fs::read(outside.path().join("keep")).unwrap(), b"outside");
}

#[cfg(unix)]
fn directory_link(target: &Path, link: &Path) {
    std::os::unix::fs::symlink(target, link).unwrap();
}
#[cfg(windows)]
fn directory_link(target: &Path, link: &Path) {
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", "New-Item -ItemType Junction -Path $env:RISUNEST_TEST_LINK -Target $env:RISUNEST_TEST_TARGET -ErrorAction Stop | Out-Null"])
        .env("RISUNEST_TEST_LINK", link)
        .env("RISUNEST_TEST_TARGET", target)
        .status().unwrap();
    assert!(status.success());
}

#[test]
fn cache_management_rejects_a_linked_cache_root_and_keeps_external_files() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let external = super::super::cache::Cache::open(outside.path()).unwrap();
    let hash = external.put(b"synthetic external object").unwrap();
    fs::create_dir(root.path().join("server-sync")).unwrap();
    directory_link(
        outside.path(),
        &root.path().join("server-sync").join("a".repeat(64)),
    );
    for clean in [false, true] {
        assert!(cache_usage(root.path(), None, &BTreeSet::new(), None, clean).is_err());
    }
    assert!(external.cas.open_object(&hash).unwrap().is_some());
}

#[test]
fn automatic_cleanup_only_removes_canonical_tombstones_and_retries_failures() {
    let root = tempfile::tempdir().unwrap();
    let (id, _) = backup(root.path(), 1, true);
    let (pinned_id, _) = backup(root.path(), 2, true);
    let (complete, complete_size) = backup(root.path(), 3, true);
    let (incomplete, incomplete_size) = backup(root.path(), 4, false);
    let base = root.path().join("server-sync/backups");
    for id in [&id, &pinned_id] {
        let path = base.join(format!(".deleting-{id}"));
        fs::rename(base.join(id), &path).unwrap();
        fs::remove_file(path.join("complete.json")).unwrap();
    }
    let invalid = base.join(".deleting-invalid");
    fs::create_dir(&invalid).unwrap();
    fs::write(invalid.join("keep"), b"unapproved").unwrap();
    assert!(cleanup_deleted_backups(root.path(), Some("busy"), &BTreeSet::new()).is_err());
    let first =
        cleanup_deleted_backups(root.path(), None, &BTreeSet::from([pinned_id.clone()])).unwrap();
    assert_eq!((first.removed, first.failed), (1, 1));
    assert!(!base.join(format!(".deleting-{id}")).exists());
    let second = cleanup_deleted_backups(root.path(), None, &BTreeSet::new()).unwrap();
    assert_eq!((second.removed, second.failed), (1, 0));
    let third = cleanup_deleted_backups(root.path(), None, &BTreeSet::new()).unwrap();
    assert_eq!((third.removed, third.failed), (0, 0));
    assert_eq!(tree_bytes(&base.join(complete)).unwrap(), complete_size);
    assert_eq!(tree_bytes(&base.join(incomplete)).unwrap(), incomplete_size);
    assert_eq!(fs::read(invalid.join("keep")).unwrap(), b"unapproved");
}

#[test]
fn automatic_cleanup_refuses_links_without_blocking_other_tombstones() {
    let root = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    fs::write(external.path().join("keep"), b"external").unwrap();
    let (id, _) = backup(root.path(), 1, true);
    let base = root.path().join("server-sync/backups");
    fs::rename(base.join(&id), base.join(format!(".deleting-{id}"))).unwrap();
    let linked = base.join(format!(".deleting-{}", uuid::Uuid::from_u128(2)));
    directory_link(external.path(), &linked);
    let result = cleanup_deleted_backups(root.path(), None, &BTreeSet::new()).unwrap();
    assert_eq!((result.removed, result.failed), (1, 1));
    assert_eq!(fs::read(external.path().join("keep")).unwrap(), b"external");
    assert!(linked.exists());
}
