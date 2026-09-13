use super::{managed_file, ManagedFileKind};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DestinationProgress {
    pub(crate) copied_bytes: u64,
    pub(crate) total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DestinationWriteResult {
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

#[derive(Debug)]
pub(crate) enum DestinationWriteError {
    InvalidSource,
    InvalidDestination,
    Cancelled,
    Io {
        operation: &'static str,
        source: io::Error,
    },
}

// Test-facing wrapper over the commit-aware writers below.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn write_desktop_destination(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
    is_cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(DestinationProgress),
) -> Result<DestinationWriteResult, DestinationWriteError> {
    write_desktop_destination_with(
        &RealFileSystem,
        source_root,
        source,
        destination_root,
        destination,
        is_cancelled,
        on_progress,
    )
}

pub(crate) fn write_desktop_destination_controlled(
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
    is_cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(DestinationProgress),
    before_replace: impl FnOnce() -> Result<(), DestinationWriteError>,
) -> Result<DestinationWriteResult, DestinationWriteError> {
    write_desktop_destination_with_commit(
        &RealFileSystem,
        source_root,
        source,
        destination_root,
        destination,
        is_cancelled,
        on_progress,
        before_replace,
    )
}

// Every per-kind controlled writer shares one delegation body; only the
// SourceKind (and with it the filename allowlist) varies. Public names and
// signatures are unchanged.
macro_rules! destination_controlled_writer {
    ($(#[$meta:meta])* $name:ident, $kind:expr) => {
        $(#[$meta])*
        pub(crate) fn $name(
            source_root: &Path,
            source: &Path,
            destination_root: &Path,
            destination: &Path,
            is_cancelled: impl Fn() -> bool,
            on_progress: impl FnMut(DestinationProgress),
            before_replace: impl FnOnce() -> Result<(), DestinationWriteError>,
        ) -> Result<DestinationWriteResult, DestinationWriteError> {
            write_desktop_destination_with_commit_kind(
                &RealFileSystem,
                $kind,
                source_root,
                source,
                destination_root,
                destination,
                is_cancelled,
                on_progress,
                before_replace,
            )
        }
    };
}

destination_controlled_writer!(
    write_screenshot_destination_controlled,
    SourceKind::ScreenshotOutput
);
destination_controlled_writer!(
    write_portable_destination_controlled,
    SourceKind::PortableBackup
);
destination_controlled_writer!(
    write_legacy_backup_destination_controlled,
    SourceKind::LegacyBackup
);
destination_controlled_writer!(
    write_charx_destination_controlled,
    SourceKind::CharacterCharX
);
destination_controlled_writer!(
    write_character_card_destination_controlled,
    SourceKind::CharacterCard
);
destination_controlled_writer!(
    write_risu_module_destination_controlled,
    SourceKind::RisuModule
);

trait DestinationFileSystem {
    type Source: Read;
    type Destination: Write;

    fn source_length(&self, path: &Path) -> io::Result<u64>;
    fn open_source(&self, path: &Path) -> io::Result<Self::Source>;
    fn create_temporary(&self, path: &Path) -> io::Result<Self::Destination>;
    fn sync_temporary(&self, destination: &mut Self::Destination) -> io::Result<()>;
    fn close_temporary(&self, destination: Self::Destination) -> io::Result<()>;
    fn close_source(&self, source: Self::Source) -> io::Result<()>;
    fn replace(&self, source: &Path, destination: &Path) -> io::Result<()>;
    fn remove_temporary(&self, path: &Path) -> io::Result<()>;
}

struct RealFileSystem;

impl DestinationFileSystem for RealFileSystem {
    type Source = File;
    type Destination = File;

    fn source_length(&self, path: &Path) -> io::Result<u64> {
        Ok(fs::metadata(path)?.len())
    }

    fn open_source(&self, path: &Path) -> io::Result<Self::Source> {
        File::open(path)
    }

    fn create_temporary(&self, path: &Path) -> io::Result<Self::Destination> {
        OpenOptions::new().write(true).create_new(true).open(path)
    }

    fn sync_temporary(&self, destination: &mut Self::Destination) -> io::Result<()> {
        destination.sync_all()
    }

    fn close_temporary(&self, destination: Self::Destination) -> io::Result<()> {
        close_file(destination)
    }

    fn close_source(&self, source: Self::Source) -> io::Result<()> {
        close_file(source)
    }

    fn replace(&self, source: &Path, destination: &Path) -> io::Result<()> {
        atomic_replace(source, destination)
    }

    fn remove_temporary(&self, path: &Path) -> io::Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn write_desktop_destination_with<F: DestinationFileSystem>(
    file_system: &F,
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
    is_cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(DestinationProgress),
) -> Result<DestinationWriteResult, DestinationWriteError> {
    write_desktop_destination_with_commit(
        file_system,
        source_root,
        source,
        destination_root,
        destination,
        is_cancelled,
        on_progress,
        || Ok(()),
    )
}

fn write_desktop_destination_with_commit<F: DestinationFileSystem>(
    file_system: &F,
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
    is_cancelled: impl Fn() -> bool,
    on_progress: impl FnMut(DestinationProgress),
    before_replace: impl FnOnce() -> Result<(), DestinationWriteError>,
) -> Result<DestinationWriteResult, DestinationWriteError> {
    write_desktop_destination_with_commit_kind(
        file_system,
        SourceKind::RisuSave,
        source_root,
        source,
        destination_root,
        destination,
        is_cancelled,
        on_progress,
        before_replace,
    )
}

#[derive(Clone, Copy)]
enum SourceKind {
    RisuSave,
    ScreenshotOutput,
    PortableBackup,
    LegacyBackup,
    CharacterCharX,
    CharacterCard,
    RisuModule,
}

impl SourceKind {
    // The exact per-kind filename allowlists. None means the RisuSave
    // managed-file check applies instead of a fixed name.
    fn allowed_file_names(self) -> Option<&'static [&'static str]> {
        match self {
            Self::RisuSave => None,
            Self::ScreenshotOutput => Some(&["archive.zip.part"]),
            Self::LegacyBackup => Some(&["archive.bin.part"]),
            Self::PortableBackup => Some(&["archive.risunest.part"]),
            Self::CharacterCharX => Some(&["character.charx", "character.jpeg"]),
            Self::CharacterCard => Some(&["character.json", "character.png"]),
            Self::RisuModule => Some(&["module.risum"]),
        }
    }
}

fn write_desktop_destination_with_commit_kind<F: DestinationFileSystem>(
    file_system: &F,
    source_kind: SourceKind,
    source_root: &Path,
    source: &Path,
    destination_root: &Path,
    destination: &Path,
    is_cancelled: impl Fn() -> bool,
    mut on_progress: impl FnMut(DestinationProgress),
    before_replace: impl FnOnce() -> Result<(), DestinationWriteError>,
) -> Result<DestinationWriteResult, DestinationWriteError> {
    let source = match source_kind.allowed_file_names() {
        None => validated_source(source_root, source)?,
        Some(allowed) => validated_named_source(source_root, source, allowed)?,
    };
    let destination = validated_destination(destination_root, destination)?;
    if source.parent() == destination.parent() {
        return Err(DestinationWriteError::InvalidDestination);
    }
    let total_bytes = file_system
        .source_length(&source)
        .map_err(|source| io_error("inspect source", source))?;
    let mut source_file = file_system
        .open_source(&source)
        .map_err(|source| io_error("open source", source))?;
    let temporary = unique_temporary_path(&destination);
    let mut destination_file = match file_system.create_temporary(&temporary) {
        Ok(file) => file,
        Err(source) => {
            let _ = file_system.close_source(source_file);
            return Err(io_error("create destination temporary file", source));
        }
    };
    let mut temporary_guard = TemporaryGuard::new(temporary.clone());
    let mut buffer = vec![0; COPY_BUFFER_BYTES];
    let mut copied_bytes = 0u64;
    let mut hasher = Sha256::new();
    on_progress(DestinationProgress {
        copied_bytes,
        total_bytes,
    });

    let mut primary_error = loop {
        if is_cancelled() {
            break Some(DestinationWriteError::Cancelled);
        }
        let bytes_read = match source_file.read(&mut buffer) {
            Ok(bytes_read) => bytes_read,
            Err(source) => break Some(io_error("read source", source)),
        };
        if bytes_read == 0 {
            break None;
        }
        let mut offset = 0;
        let mut write_error = None;
        while offset < bytes_read {
            if is_cancelled() {
                break;
            }
            match destination_file.write(&buffer[offset..bytes_read]) {
                Ok(0) => {
                    break;
                }
                Ok(bytes_written) => {
                    offset += bytes_written;
                }
                Err(source) => {
                    write_error = Some(io_error("write destination temporary file", source));
                    break;
                }
            }
        }
        if let Some(error) = write_error {
            break Some(error);
        }
        if offset < bytes_read {
            if is_cancelled() {
                break Some(DestinationWriteError::Cancelled);
            }
            break Some(io_error(
                "write destination temporary file",
                io::Error::new(
                    io::ErrorKind::WriteZero,
                    "destination write returned zero bytes",
                ),
            ));
        }
        copied_bytes += bytes_read as u64;
        hasher.update(&buffer[..bytes_read]);
        on_progress(DestinationProgress {
            copied_bytes,
            total_bytes,
        });
    };
    if primary_error.is_none() {
        if let Err(source) = destination_file.flush() {
            primary_error = Some(io_error("flush destination temporary file", source));
        }
    }
    if primary_error.is_none() {
        if let Err(source) = file_system.sync_temporary(&mut destination_file) {
            primary_error = Some(io_error("sync destination temporary file", source));
        }
    }
    if let Err(source) = file_system.close_temporary(destination_file) {
        if primary_error.is_none() {
            primary_error = Some(io_error("close destination temporary file", source));
        }
    }
    if let Err(source) = file_system.close_source(source_file) {
        if primary_error.is_none() {
            primary_error = Some(io_error("close source", source));
        }
    }
    if primary_error.is_none() && is_cancelled() {
        primary_error = Some(DestinationWriteError::Cancelled);
    }
    if primary_error.is_none() {
        if let Err(error) = before_replace() {
            primary_error = Some(error);
        }
    }
    if primary_error.is_none() {
        if let Err(source) = file_system.replace(&temporary, &destination) {
            primary_error = Some(io_error("replace destination", source));
        } else {
            temporary_guard.disarm();
        }
    }
    if let Some(error) = primary_error {
        let _ = file_system.remove_temporary(&temporary);
        return Err(error);
    }

    Ok(DestinationWriteResult {
        bytes: copied_bytes,
        sha256: hex::encode(hasher.finalize()),
    })
}

// One shared validator enforces the path-containment boundary (canonical
// root, direct child of it, regular file) for every named export source;
// only the per-kind filename allowlist varies. The RisuSave managed-file
// check stays in validated_source below.
fn validated_named_source(
    source_root: &Path,
    source: &Path,
    allowed_file_names: &[&str],
) -> Result<PathBuf, DestinationWriteError> {
    let root = fs::canonicalize(source_root).map_err(|_| DestinationWriteError::InvalidSource)?;
    let source = fs::canonicalize(source).map_err(|_| DestinationWriteError::InvalidSource)?;
    let name = source.file_name().and_then(|name| name.to_str());
    if source.parent() != Some(root.as_path())
        || !name.is_some_and(|name| allowed_file_names.contains(&name))
        || !source.is_file()
    {
        return Err(DestinationWriteError::InvalidSource);
    }
    Ok(source)
}

fn validated_source(source_root: &Path, source: &Path) -> Result<PathBuf, DestinationWriteError> {
    let root = fs::canonicalize(source_root).map_err(|_| DestinationWriteError::InvalidSource)?;
    let source = fs::canonicalize(source).map_err(|_| DestinationWriteError::InvalidSource)?;
    if source.parent() != Some(root.as_path())
        || !matches!(managed_file(&source), Some((_, ManagedFileKind::Completed)))
        || !source.is_file()
    {
        return Err(DestinationWriteError::InvalidSource);
    }
    Ok(source)
}

fn validated_destination(
    destination_root: &Path,
    destination: &Path,
) -> Result<PathBuf, DestinationWriteError> {
    let root = fs::canonicalize(destination_root)
        .map_err(|_| DestinationWriteError::InvalidDestination)?;
    let parent = destination
        .parent()
        .and_then(|parent| fs::canonicalize(parent).ok())
        .ok_or(DestinationWriteError::InvalidDestination)?;
    let file_name = destination
        .file_name()
        .ok_or(DestinationWriteError::InvalidDestination)?;
    if parent != root || file_name == "." || file_name == ".." {
        return Err(DestinationWriteError::InvalidDestination);
    }
    let destination = root.join(file_name);
    if destination == root {
        return Err(DestinationWriteError::InvalidDestination);
    }
    Ok(destination)
}

fn unique_temporary_path(destination: &Path) -> PathBuf {
    destination.with_file_name(format!("{}.tmp", Uuid::new_v4()))
}

fn io_error(operation: &'static str, source: io::Error) -> DestinationWriteError {
    DestinationWriteError::Io { operation, source }
}

#[cfg(windows)]
fn close_file(file: File) -> io::Result<()> {
    use std::ffi::c_void;
    use std::os::windows::io::IntoRawHandle;

    #[link(name = "kernel32")]
    extern "system" {
        fn CloseHandle(handle: *mut c_void) -> i32;
    }

    let handle = file.into_raw_handle();
    let closed = unsafe { CloseHandle(handle.cast()) };
    if closed == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn close_file(file: File) -> io::Result<()> {
    use std::os::fd::IntoRawFd;

    extern "C" {
        fn close(file_descriptor: i32) -> i32;
    }

    let file_descriptor = file.into_raw_fd();
    let closed = unsafe { close(file_descriptor) };
    if closed == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(any(windows, unix)))]
fn close_file(file: File) -> io::Result<()> {
    drop(file);
    Ok(())
}

#[cfg(windows)]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let replaced = unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn atomic_replace(source: &Path, destination: &Path) -> io::Result<()> {
    fs::rename(source, destination)
}

struct TemporaryGuard {
    path: PathBuf,
    armed: bool,
}

impl TemporaryGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::io::{Read, Write};
    use tempfile::TempDir;
    use uuid::Uuid;

    struct ChunkedReader {
        file: File,
        maximum: usize,
        fail_after: Option<usize>,
        bytes_read: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self
                .fail_after
                .is_some_and(|fail_after| self.bytes_read >= fail_after)
            {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected read failure",
                ));
            }
            let length = buffer.len().min(self.maximum);
            let bytes_read = self.file.read(&mut buffer[..length])?;
            self.bytes_read += bytes_read;
            Ok(bytes_read)
        }
    }

    struct ChunkedWriter {
        file: File,
        maximum: usize,
        fail_after: Option<usize>,
        bytes_written: usize,
        fail_flush: bool,
    }

    impl Write for ChunkedWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if self
                .fail_after
                .is_some_and(|fail_after| self.bytes_written >= fail_after)
            {
                return Err(io::Error::new(io::ErrorKind::Other, "injected disk full"));
            }
            let remaining = self
                .fail_after
                .map(|fail_after| fail_after.saturating_sub(self.bytes_written))
                .unwrap_or(usize::MAX);
            let length = buffer.len().min(self.maximum).min(remaining);
            if length == 0 {
                return Err(io::Error::new(io::ErrorKind::Other, "injected disk full"));
            }
            let bytes_written = self.file.write(&buffer[..length])?;
            self.bytes_written += bytes_written;
            Ok(bytes_written)
        }

        fn flush(&mut self) -> io::Result<()> {
            if self.fail_flush {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected flush failure",
                ));
            }
            self.file.flush()
        }
    }

    #[derive(Debug, Clone, Copy)]
    enum Fault {
        CreateUnownedCollision,
        Read,
        DiskFull,
        Flush,
        Sync,
        CloseTemporary,
        CloseSource,
        Replace,
    }

    struct ChunkedFileSystem {
        maximum_read: usize,
        maximum_write: usize,
        fault: Option<Fault>,
    }

    impl DestinationFileSystem for ChunkedFileSystem {
        type Source = ChunkedReader;
        type Destination = ChunkedWriter;

        fn source_length(&self, path: &Path) -> io::Result<u64> {
            Ok(fs::metadata(path)?.len())
        }

        fn open_source(&self, path: &Path) -> io::Result<Self::Source> {
            Ok(ChunkedReader {
                file: File::open(path)?,
                maximum: self.maximum_read,
                fail_after: matches!(self.fault, Some(Fault::Read)).then_some(1_000),
                bytes_read: 0,
            })
        }

        fn create_temporary(&self, path: &Path) -> io::Result<Self::Destination> {
            if matches!(self.fault, Some(Fault::CreateUnownedCollision)) {
                fs::write(path, b"unowned sibling")?;
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "injected temporary collision",
                ));
            }
            Ok(ChunkedWriter {
                file: OpenOptions::new().write(true).create_new(true).open(path)?,
                maximum: self.maximum_write,
                fail_after: matches!(self.fault, Some(Fault::DiskFull)).then_some(1_000),
                bytes_written: 0,
                fail_flush: matches!(self.fault, Some(Fault::Flush)),
            })
        }

        fn sync_temporary(&self, destination: &mut Self::Destination) -> io::Result<()> {
            if matches!(self.fault, Some(Fault::Sync)) {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected sync failure",
                ));
            }
            destination.file.sync_all()
        }

        fn close_temporary(&self, destination: Self::Destination) -> io::Result<()> {
            drop(destination);
            if matches!(self.fault, Some(Fault::CloseTemporary)) {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected close failure",
                ))
            } else {
                Ok(())
            }
        }

        fn close_source(&self, source: Self::Source) -> io::Result<()> {
            drop(source);
            if matches!(self.fault, Some(Fault::CloseSource)) {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected close failure",
                ))
            } else {
                Ok(())
            }
        }

        fn replace(&self, source: &Path, destination: &Path) -> io::Result<()> {
            if matches!(self.fault, Some(Fault::Replace)) {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected rename failure",
                ))
            } else {
                atomic_replace(source, destination)
            }
        }

        fn remove_temporary(&self, path: &Path) -> io::Result<()> {
            fs::remove_file(path)
        }
    }

    fn source_path(root: &Path) -> std::path::PathBuf {
        root.join(format!("risusave-{}.risudat", Uuid::new_v4()))
    }

    fn sibling_temporary_files(root: &Path) -> Vec<std::path::PathBuf> {
        fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "tmp"))
            .collect()
    }

    #[test]
    fn copies_exact_bytes_and_reports_bounded_progress() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("exports");
        let destination_root = directory.path().join("chosen");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = source_path(&source_root);
        let destination = destination_root.join("backup.risudat");
        let bytes = (0..200_000)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        fs::write(&source, &bytes).unwrap();
        let mut progress = Vec::new();

        let result = write_desktop_destination(
            &source_root,
            &source,
            &destination_root,
            &destination,
            || false,
            |update| progress.push(update),
        )
        .unwrap();

        assert_eq!(result.bytes, bytes.len() as u64);
        assert_eq!(
            result.sha256,
            "e24bc62381f1224fbbb74688663f8f9743b9680b193edd666835e97b06e730eb"
        );
        assert_eq!(fs::read(destination).unwrap(), bytes);
        assert_eq!(progress.first().unwrap().copied_bytes, 0);
        assert_eq!(progress.last().unwrap().copied_bytes, bytes.len() as u64);
        assert!(progress.windows(2).all(|pair| {
            pair[0].copied_bytes <= pair[1].copied_bytes
                && pair[1].copied_bytes - pair[0].copied_bytes <= 64 * 1024
        }));
        assert!(progress
            .iter()
            .all(|update| update.total_bytes == bytes.len() as u64));
    }

    #[test]
    fn atomically_replaces_an_existing_same_name_destination() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("exports");
        let destination_root = directory.path().join("chosen");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = source_path(&source_root);
        let destination = destination_root.join("backup.risudat");
        fs::write(&source, b"complete new export").unwrap();
        fs::write(&destination, b"previous export").unwrap();

        write_desktop_destination(
            &source_root,
            &source,
            &destination_root,
            &destination,
            || false,
            |_| {},
        )
        .unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"complete new export");
        assert!(sibling_temporary_files(&destination_root).is_empty());
    }

    #[test]
    fn writes_a_long_valid_destination_name_without_expanding_the_temp_component() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("exports");
        let destination_root = directory.path().join("chosen");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = source_path(&source_root);
        let destination = destination_root.join(format!("{}.risudat", "a".repeat(240)));
        fs::write(&source, b"long destination export").unwrap();

        write_desktop_destination(
            &source_root,
            &source,
            &destination_root,
            &destination,
            || false,
            |_| {},
        )
        .unwrap();

        assert_eq!(fs::read(&destination).unwrap(), b"long destination export");
        assert!(sibling_temporary_files(&destination_root).is_empty());
    }

    #[test]
    fn handles_partial_reads_and_partial_writes_without_changing_bytes() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("exports");
        let destination_root = directory.path().join("chosen");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = source_path(&source_root);
        let destination = destination_root.join("backup.risudat");
        let bytes = (0..150_000)
            .map(|index| (index % 241) as u8)
            .collect::<Vec<_>>();
        fs::write(&source, &bytes).unwrap();
        let file_system = ChunkedFileSystem {
            maximum_read: 701,
            maximum_write: 113,
            fault: None,
        };

        write_desktop_destination_with(
            &file_system,
            &source_root,
            &source,
            &destination_root,
            &destination,
            || false,
            |_| {},
        )
        .unwrap();

        assert_eq!(fs::read(destination).unwrap(), bytes);
    }

    #[test]
    fn disk_full_failure_preserves_existing_destination_and_removes_owned_temporary() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("exports");
        let destination_root = directory.path().join("chosen");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = source_path(&source_root);
        let destination = destination_root.join("backup.risudat");
        fs::write(&source, vec![7; 200_000]).unwrap();
        fs::write(&destination, b"previous export").unwrap();
        let file_system = ChunkedFileSystem {
            maximum_read: COPY_BUFFER_BYTES,
            maximum_write: COPY_BUFFER_BYTES,
            fault: Some(Fault::DiskFull),
        };

        let error = write_desktop_destination_with(
            &file_system,
            &source_root,
            &source,
            &destination_root,
            &destination,
            || false,
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DestinationWriteError::Io {
                operation: "write destination temporary file",
                ..
            }
        ));
        assert_eq!(fs::read(&destination).unwrap(), b"previous export");
        assert!(sibling_temporary_files(&destination_root).is_empty());
    }

    #[test]
    fn cancellation_preserves_existing_destination_and_removes_owned_temporary() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("exports");
        let destination_root = directory.path().join("chosen");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = source_path(&source_root);
        let destination = destination_root.join("backup.risudat");
        fs::write(&source, vec![9; COPY_BUFFER_BYTES * 3]).unwrap();
        fs::write(&destination, b"previous export").unwrap();
        let cancelled = Cell::new(false);

        let error = write_desktop_destination(
            &source_root,
            &source,
            &destination_root,
            &destination,
            || cancelled.get(),
            |progress| {
                if progress.copied_bytes > 0 {
                    cancelled.set(true);
                }
            },
        )
        .unwrap_err();

        assert!(matches!(error, DestinationWriteError::Cancelled));
        assert_eq!(fs::read(&destination).unwrap(), b"previous export");
        assert!(sibling_temporary_files(&destination_root).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn large_sparse_export_disk_full_preserves_the_previous_windows_destination() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("exports");
        let destination_root = directory.path().join("chosen");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = source_path(&source_root);
        let destination = destination_root.join("large-backup.risudat");
        File::create(&source)
            .unwrap()
            .set_len(65 * 1024 * 1024 + 1)
            .unwrap();
        fs::write(&destination, b"previous large export").unwrap();
        let file_system = ChunkedFileSystem {
            maximum_read: COPY_BUFFER_BYTES,
            maximum_write: COPY_BUFFER_BYTES,
            fault: Some(Fault::DiskFull),
        };

        let error = write_desktop_destination_with(
            &file_system,
            &source_root,
            &source,
            &destination_root,
            &destination,
            || false,
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DestinationWriteError::Io {
                operation: "write destination temporary file",
                ..
            }
        ));
        assert_eq!(fs::read(destination).unwrap(), b"previous large export");
        assert!(sibling_temporary_files(&destination_root).is_empty());
    }

    #[test]
    fn cancellation_winning_the_finalization_race_preserves_the_existing_destination() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("exports");
        let destination_root = directory.path().join("chosen");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = source_path(&source_root);
        let destination = destination_root.join("backup.risudat");
        fs::write(&source, vec![13; COPY_BUFFER_BYTES * 2]).unwrap();
        fs::write(&destination, b"previous export").unwrap();

        let error = write_desktop_destination_controlled(
            &source_root,
            &source,
            &destination_root,
            &destination,
            || false,
            |_| {},
            || Err(DestinationWriteError::Cancelled),
        )
        .unwrap_err();

        assert!(matches!(error, DestinationWriteError::Cancelled));
        assert_eq!(fs::read(&destination).unwrap(), b"previous export");
        assert!(sibling_temporary_files(&destination_root).is_empty());
    }

    #[test]
    fn legacy_backup_failure_preserves_the_existing_destination() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("jobs");
        let destination_root = directory.path().join("chosen");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = source_root.join("archive.bin.part");
        let destination = destination_root.join("risu-backup.bin");
        fs::write(&source, vec![13; COPY_BUFFER_BYTES * 2]).unwrap();
        fs::write(&destination, b"previous backup").unwrap();

        let error = write_legacy_backup_destination_controlled(
            &source_root,
            &source,
            &destination_root,
            &destination,
            || false,
            |_| {},
            || Err(DestinationWriteError::Cancelled),
        )
        .unwrap_err();

        assert!(matches!(error, DestinationWriteError::Cancelled));
        assert_eq!(fs::read(&destination).unwrap(), b"previous backup");
        assert!(sibling_temporary_files(&destination_root).is_empty());
    }

    #[test]
    fn io_failures_preserve_existing_destination_and_remove_owned_temporary() {
        for (fault, expected_operation) in [
            (Fault::Read, "read source"),
            (Fault::Flush, "flush destination temporary file"),
            (Fault::Sync, "sync destination temporary file"),
            (Fault::CloseTemporary, "close destination temporary file"),
            (Fault::CloseSource, "close source"),
            (Fault::Replace, "replace destination"),
        ] {
            let directory = TempDir::new().unwrap();
            let source_root = directory.path().join("exports");
            let destination_root = directory.path().join("chosen");
            fs::create_dir_all(&source_root).unwrap();
            fs::create_dir_all(&destination_root).unwrap();
            let source = source_path(&source_root);
            let destination = destination_root.join("backup.risudat");
            fs::write(&source, vec![11; COPY_BUFFER_BYTES * 2]).unwrap();
            fs::write(&destination, b"previous export").unwrap();
            let file_system = ChunkedFileSystem {
                maximum_read: COPY_BUFFER_BYTES,
                maximum_write: COPY_BUFFER_BYTES,
                fault: Some(fault),
            };

            let error = write_desktop_destination_with(
                &file_system,
                &source_root,
                &source,
                &destination_root,
                &destination,
                || false,
                |_| {},
            )
            .unwrap_err();

            assert!(
                matches!(
                    error,
                    DestinationWriteError::Io { operation, .. }
                        if operation == expected_operation
                ),
                "unexpected error for {fault:?}: {error:?}"
            );
            assert_eq!(fs::read(&destination).unwrap(), b"previous export");
            assert!(sibling_temporary_files(&destination_root).is_empty());
        }
    }

    #[test]
    fn rejects_unmanaged_sources_and_destinations_outside_the_selected_root() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("exports");
        let destination_root = directory.path().join("chosen");
        let outside = directory.path().join("outside");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let unmanaged_source = source_root.join("manual.risudat");
        let managed_outside_source = source_path(&outside);
        fs::write(&unmanaged_source, b"unmanaged").unwrap();
        fs::write(&managed_outside_source, b"outside").unwrap();

        for source in [&unmanaged_source, &managed_outside_source] {
            let error = write_desktop_destination(
                &source_root,
                source,
                &destination_root,
                &destination_root.join("backup.risudat"),
                || false,
                |_| {},
            )
            .unwrap_err();
            assert!(matches!(error, DestinationWriteError::InvalidSource));
        }

        let source = source_path(&source_root);
        fs::write(&source, b"managed").unwrap();
        let error = write_desktop_destination(
            &source_root,
            &source,
            &destination_root,
            &outside.join("backup.risudat"),
            || false,
            |_| {},
        )
        .unwrap_err();
        assert!(matches!(error, DestinationWriteError::InvalidDestination));
        assert!(!outside.join("backup.risudat").exists());
    }

    #[test]
    fn temporary_name_collision_does_not_delete_an_unowned_file() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("exports");
        let destination_root = directory.path().join("chosen");
        fs::create_dir_all(&source_root).unwrap();
        fs::create_dir_all(&destination_root).unwrap();
        let source = source_path(&source_root);
        let destination = destination_root.join("backup.risudat");
        fs::write(&source, b"managed").unwrap();
        fs::write(&destination, b"previous export").unwrap();
        let file_system = ChunkedFileSystem {
            maximum_read: COPY_BUFFER_BYTES,
            maximum_write: COPY_BUFFER_BYTES,
            fault: Some(Fault::CreateUnownedCollision),
        };

        let error = write_desktop_destination_with(
            &file_system,
            &source_root,
            &source,
            &destination_root,
            &destination,
            || false,
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(
            error,
            DestinationWriteError::Io {
                operation: "create destination temporary file",
                ..
            }
        ));
        let temporary_files = sibling_temporary_files(&destination_root);
        assert_eq!(temporary_files.len(), 1);
        assert_eq!(fs::read(&temporary_files[0]).unwrap(), b"unowned sibling");
        assert_eq!(fs::read(destination).unwrap(), b"previous export");
    }

    #[test]
    fn rejects_the_app_private_export_directory_as_a_destination_root() {
        let directory = TempDir::new().unwrap();
        let source_root = directory.path().join("exports");
        fs::create_dir_all(&source_root).unwrap();
        let source = source_path(&source_root);
        fs::write(&source, b"managed export").unwrap();

        let error = write_desktop_destination(
            &source_root,
            &source,
            &source_root,
            &source,
            || false,
            |_| {},
        )
        .unwrap_err();

        assert!(matches!(error, DestinationWriteError::InvalidDestination));
        assert_eq!(fs::read(source).unwrap(), b"managed export");
        assert!(sibling_temporary_files(&source_root).is_empty());
    }
}
