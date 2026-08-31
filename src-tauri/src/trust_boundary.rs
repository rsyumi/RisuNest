//! Shared trust-boundary primitives.
//!
//! These helpers guard filesystem and identifier validation at process
//! boundaries and were previously copied per subsystem; every consumer must
//! keep the exact same semantics, so they live here once.

use std::fs::Metadata;
use std::io;
use std::path::Path;

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
