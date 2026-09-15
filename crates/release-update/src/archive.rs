use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveEntryKind {
    File,
    Directory,
    Symlink,
    Hardlink,
    Device,
    Fifo,
    Other,
}

pub fn validate_archive_entry(name: &str, kind: ArchiveEntryKind) -> Result<()> {
    if !matches!(kind, ArchiveEntryKind::File | ArchiveEntryKind::Directory) {
        return Err(Error::UnsafeArchiveEntry(format!(
            "unsupported entry type for {name}"
        )));
    }
    safe_archive_path(Path::new("."), name).map(|_| ())
}

pub fn safe_archive_path(root: &Path, entry_name: &str) -> Result<PathBuf> {
    if entry_name.is_empty()
        || entry_name.contains('\0')
        || entry_name.contains('\\')
        || entry_name.contains(':')
        || entry_name.starts_with('/')
    {
        return Err(Error::UnsafeArchiveEntry(entry_name.to_owned()));
    }
    let relative = Path::new(entry_name);
    if relative.as_os_str().is_empty() {
        return Err(Error::UnsafeArchiveEntry(entry_name.to_owned()));
    }
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            return Err(Error::UnsafeArchiveEntry(entry_name.to_owned()));
        }
    }
    Ok(root.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_lexically_safe_regular_entries() {
        assert_eq!(
            safe_archive_path(Path::new("stage"), "bin/risunest-sync").unwrap(),
            Path::new("stage/bin/risunest-sync")
        );
        for unsafe_name in ["", "../escape", "/absolute", "C:/drive", "a\\b", "./same"] {
            assert!(safe_archive_path(Path::new("stage"), unsafe_name).is_err());
        }
        assert!(validate_archive_entry("link", ArchiveEntryKind::Symlink).is_err());
        assert!(validate_archive_entry("file", ArchiveEntryKind::File).is_ok());
    }
}
