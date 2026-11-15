use std::{fs, io, path::{Component, Path, PathBuf}};

pub(super) type Result<T> = std::result::Result<T, String>;

/// Returns a cleanup code after logging the failure behind it. The line stays
/// in memory, since the log directory can be one of the roots being removed.
#[track_caller]
pub(super) fn failed(code: &str, cause: impl std::fmt::Display) -> String {
    let at = std::panic::Location::caller();
    crate::native_log::log_ring_only(
        "error",
        "app-cleanup",
        format!("{code} cause={} at={}:{}", crate::native_log::failure_text(&cause), at.file(), at.line()),
    );
    code.to_owned()
}

#[cfg(mobile)]
#[track_caller]
pub(super) fn refused(code: &str, error: &impl crate::native_log::CommandFailure) -> String {
    match error.detail() {
        Some(detail) => failed(code, format!("{} {detail}", error.code())),
        None => failed(code, error.code()),
    }
}

pub(super) fn linked(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    { metadata.file_type().is_symlink() }
}

/// A cleanup root must be an absolute, dedicated path with no redirected ancestors.
pub(super) fn validate(path: &Path) -> Result<()> {
    validate_from(path, path)
}

/// A data root entry that is itself a link: it is validated as a root, except
/// that its own last component may be the link.
pub(super) fn validate_link(link: &Path) -> Result<()> {
    validate_from(link, link.parent().ok_or("cleanup-path-invalid")?)
}

/// The directory a data root link leads to. It is canonical, so no component
/// is redirected, but it may sit directly under a drive or filesystem root.
pub(super) fn validate_target(target: &Path) -> Result<()> {
    if !target.is_absolute() || target.components().any(|part| matches!(part, Component::ParentDir | Component::CurDir)) {
        return Err("cleanup-path-invalid".into());
    }
    unredirected(target)
}

fn validate_from(path: &Path, checked: &Path) -> Result<()> {
    if !path.is_absolute() || path.parent().and_then(Path::parent).is_none()
        || path.components().any(|part| matches!(part, Component::ParentDir | Component::CurDir)) {
        return Err("cleanup-path-invalid".into());
    }
    unredirected(checked)
}

fn unredirected(path: &Path) -> Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if linked(&metadata) => return Err("cleanup-path-redirected".into()),
            Ok(_) => {},
            Err(error) if error.kind() == io::ErrorKind::NotFound => {},
            Err(error) => return Err(failed("cleanup-path-unavailable", error)),
        }
    }
    Ok(())
}

pub(super) fn remove(path: &Path) -> Result<()> {
    validate(path)?;
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(failed("cleanup-path-unavailable", error)),
    };
    let result = if metadata.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) };
    result.map_err(|error| failed("cleanup-files-busy-or-denied", error))
}

/// Deletes what the app created inside a linked data root's target and leaves
/// every other entry. A removal then takes the target if nothing else is left
/// in it, and the link last, so an interrupted run finds the same target again.
pub(super) fn clear_linked(link: &Path, target: Option<&Path>, remove_link: bool) -> Result<()> {
    validate_link(link)?;
    if let Some(target) = target {
        validate_target(target)?;
        let entries = match fs::read_dir(target) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(failed("cleanup-path-unavailable", error)),
        };
        for entry in entries {
            let entry = entry.map_err(|error| failed("cleanup-path-unavailable", error))?;
            if !crate::app_paths::is_data_root_entry(&entry.file_name()) { continue; }
            remove_entry(&entry.path())?;
        }
        if remove_link && target.parent().is_some() {
            match fs::remove_dir(target) {
                Err(error) if !matches!(error.kind(), io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty) =>
                    return Err(failed("cleanup-files-busy-or-denied", error)),
                _ => {},
            }
        }
    }
    if remove_link { remove_entry(link)?; }
    Ok(())
}

/// Removes one entry without following it when it is a link.
fn remove_entry(path: &Path) -> Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(failed("cleanup-path-unavailable", error)),
    };
    let result = if linked(&metadata) {
        remove_link_itself(path, &metadata)
    } else if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    };
    result.map_err(|error| failed("cleanup-files-busy-or-denied", error))
}

fn remove_link_itself(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // A junction or directory symlink is removed as a directory.
        if metadata.file_attributes() & 0x10 != 0 { return fs::remove_dir(path); }
    }
    let _ = metadata;
    fs::remove_file(path)
}

pub(super) fn independent_roots(mut paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.sort();
    paths.dedup();
    let all = paths.clone();
    paths.retain(|path| !all.iter().any(|other| other != path && path.starts_with(other)));
    paths
}

pub(super) fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    validate(path)?;
    let parent = path.parent().ok_or("cleanup-path-invalid")?;
    fs::create_dir_all(parent).map_err(|error| failed("cleanup-journal-unavailable", error))?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|error| failed("cleanup-journal-unavailable", error))?;
    serde_json::to_writer(file.as_file_mut(), value).map_err(|error| failed("cleanup-journal-unavailable", error))?;
    file.as_file().sync_all().map_err(|error| failed("cleanup-journal-unavailable", error))?;
    file.persist(path).map_err(|error| failed("cleanup-journal-unavailable", error))?;
    #[cfg(unix)]
    fs::File::open(parent).and_then(|directory| directory.sync_all())
        .map_err(|error| failed("cleanup-journal-unavailable", error))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn journal_replacement_keeps_a_complete_retry_record() {
        let dir = tempfile::tempdir().unwrap();
        let journal = dir.path().join("control/request.json");
        write_json(&journal, &serde_json::json!({"pending": true, "error": null})).unwrap();
        let retry = serde_json::json!({"pending": true, "error": "cleanup-files-busy-or-denied"});
        write_json(&journal, &retry).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&fs::read(&journal).unwrap()).unwrap(), retry);
        assert_eq!(fs::read_dir(journal.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn removes_only_selected_roots_and_repeated_cleanup_is_safe() {
        let dir = tempfile::tempdir().unwrap();
        let owned = dir.path().join("owned");
        let exported = dir.path().join("exported");
        fs::create_dir_all(owned.join("snapshots")).unwrap();
        fs::write(owned.join("snapshots/data"), b"synthetic").unwrap();
        fs::write(&exported, b"synthetic export").unwrap();
        remove(&owned).unwrap();
        remove(&owned).unwrap();
        assert!(!owned.exists());
        assert_eq!(fs::read(exported).unwrap(), b"synthetic export");
    }

    #[test]
    fn validates_every_root_before_mutation_and_collapses_nested_roots() {
        let dir = tempfile::tempdir().unwrap();
        let owned = dir.path().join("owned");
        assert!(validate(&owned.join(".." )).is_err());
        assert!(validate(Path::new("relative/data")).is_err());
        assert_eq!(independent_roots(vec![owned.join("logs"), owned.clone(), owned.clone()]), vec![owned]);
    }

    #[cfg(unix)]
    #[test]
    fn redirected_root_is_rejected_and_nested_links_do_not_delete_the_target() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let owned = dir.path().join("owned");
        std::os::unix::fs::symlink(outside.path(), &owned).unwrap();
        assert!(remove(&owned).is_err());
        fs::remove_file(&owned).unwrap();
        fs::create_dir(&owned).unwrap();
        fs::write(outside.path().join("keep"), b"keep").unwrap();
        std::os::unix::fs::symlink(outside.path(), owned.join("link")).unwrap();
        remove(&owned).unwrap();
        assert!(outside.path().join("keep").exists());
    }
}
