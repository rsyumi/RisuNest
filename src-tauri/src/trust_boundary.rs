//! Shared trust-boundary primitives.
//!
//! These helpers guard filesystem and identifier validation at process
//! boundaries and were previously copied per subsystem; every consumer must
//! keep the exact same semantics, so they live here once.

use std::fs::Metadata;
use std::io;
use std::path::Path;

/// Open a regular source without following a final symlink/reparse point. Callers that spool
/// mutable data compare handle identity and modification metadata before and after the copy.
pub(crate) fn open_regular_source(path: &Path) -> io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || is_link_like(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source is not a regular file",
        ));
    }
    Ok(file)
}

/// A byte of the canonical lowercase hexadecimal alphabet.
pub(crate) fn is_lower_hex_byte(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

/// A canonical lowercase SHA-256 hex string (64 lowercase hex characters).
pub(crate) fn is_lower_hex_256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(is_lower_hex_byte)
}

/// Whether the metadata describes a symlink or, on Windows, any reparse
/// point (junctions included) that could redirect a trusted path.
#[cfg(windows)]
pub(crate) fn is_link_like(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
pub(crate) fn is_link_like(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

/// Durably syncs a directory's entries where the platform supports it.
/// Returns whether a sync actually happened (Windows directory handles do
/// not support fsync, so entry durability rides on the volume there).
#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> io::Result<bool> {
    std::fs::File::open(path)?.sync_all()?;
    Ok(true)
}

#[cfg(not(unix))]
pub(crate) fn sync_directory(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

/// Android forbids app hard links. Move a completed file atomically while
/// preserving an existing destination, including a dangling symlink.
/// Callers must sync the source and destination directories after success.
#[cfg(target_os = "android")]
pub(crate) fn rename_without_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    // Use the syscall without depending on bionic's API 30 renameat2 wrapper.
    // SAFETY: both C strings remain valid for the duration of the syscall.
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        // Do not fall back to rename(), which would overwrite a live object.
        Err(io::Error::last_os_error())
    }
}

/// Same-volume move with no replacement. Unlike hard-link + unlink, this updates
/// the staging name in one operation and also works on Windows filesystems without links.
#[cfg(windows)]
pub(crate) fn rename_without_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    // SAFETY: both paths are NUL terminated and live throughout the call. Flags 0
    // neither replace an existing destination nor copy across volumes.
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) } != 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
