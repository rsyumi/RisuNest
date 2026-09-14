use crate::trust_boundary::{is_link_like, is_lower_hex_256, sync_directory};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
#[cfg(windows)]
use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};

const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedPayload {
    pub content_hash: String,
    pub byte_size: u64,
    pub physical_key: String,
    pub deduplicated: bool,
    pub directory_entries_synced: bool,
}

#[derive(Debug)]
pub struct PayloadCas {
    repository_root: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExactObjectUnlink {
    Missing,
    Removed { directory_entries_synced: bool },
}

struct StagingFile {
    path: PathBuf,
    parent: PathBuf,
    owned: bool,
}

impl StagingFile {
    fn remove_and_sync(&mut self) -> io::Result<bool> {
        if !self.owned {
            return Ok(true);
        }
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        self.owned = false;
        sync_directory(&self.parent)
    }
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        if self.owned && fs::remove_file(&self.path).is_ok() {
            let _ = sync_directory(&self.parent);
        }
    }
}

impl PayloadCas {
    pub(crate) fn repository_root(&self) -> &Path {
        &self.repository_root
    }

    pub fn new(repository_root: impl AsRef<Path>) -> io::Result<Self> {
        let repository_root = absolute_path(repository_root.as_ref())?;
        reject_link_components(&repository_root)?;
        let metadata = fs::symlink_metadata(&repository_root)?;
        ensure_real_directory(&repository_root, &metadata)?;
        let repository_root = fs::canonicalize(repository_root)?;
        let cas = Self { repository_root };
        cas.ensure_repository_root()?;
        Ok(cas)
    }

    pub fn prepare_bytes(&self, data: &[u8]) -> Result<PreparedPayload, io::Error> {
        self.prepare_reader(&mut io::Cursor::new(data))
    }

    pub(crate) fn create_ipc_staging_file(&self) -> io::Result<tempfile::NamedTempFile> {
        self.ensure_repository_root()?;
        let mut directory_entries_synced = true;
        let assets_directory = self.ensure_directory(
            &self.repository_root,
            "assets-v2",
            &mut directory_entries_synced,
        )?;
        let staging_directory =
            self.ensure_directory(&assets_directory, "staging", &mut directory_entries_synced)?;
        tempfile::NamedTempFile::new_in(staging_directory)
    }

    pub fn prepare_reader(&self, reader: &mut impl Read) -> Result<PreparedPayload, io::Error> {
        self.prepare_reader_inner(reader, None)
    }

    pub(crate) fn prepare_reader_expected(
        &self,
        reader: &mut impl Read,
        expected_content_hash: &str,
        expected_byte_size: u64,
    ) -> Result<PreparedPayload, io::Error> {
        validate_content_hash(expected_content_hash)?;
        self.prepare_reader_inner(reader, Some((expected_content_hash, expected_byte_size)))
    }

    /// Adopt an immutable payload produced in this repository's private import
    /// staging. Revalidate bytes and identity, but do not rewrite or fsync the
    /// payload a second time. The parser already synced it before returning.
    pub(crate) fn adopt_import_payload(
        &self,
        path: &Path,
        expected_hash: &str,
        expected_size: u64,
        cancelled: &impl Fn() -> bool,
    ) -> io::Result<PreparedPayload> {
        validate_content_hash(expected_hash)?;
        self.ensure_repository_root()?;
        reject_link_components(path)?;
        let canonical = fs::canonicalize(path)?;
        let jobs = self.repository_root.join("native-file-jobs").join("jobs");
        if !canonical.starts_with(&jobs)
            || !matches!(
                path.extension().and_then(|s| s.to_str()),
                Some("payload" | "stage")
            )
        {
            return invalid_owned_path(path, "only private import payloads may be adopted");
        }
        let mut file = self.open_exact_owned_file(&canonical)?;
        let identity = exact_file_identity(&file)?;
        #[cfg(any(unix, windows))]
        if identity.links != 1 {
            return invalid_owned_path(path, "import staging must have exactly one link");
        }
        if identity.byte_size() != expected_size {
            return collision_or_corruption(expected_hash);
        }
        let mut hash = Sha256::new();
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        loop {
            if cancelled() {
                return Err(io::Error::other("import cancelled"));
            }
            let length = file.read(&mut buffer)?;
            if length == 0 {
                break;
            }
            hash.update(&buffer[..length]);
        }
        if hex::encode(hash.finalize()) != expected_hash || exact_file_identity(&file)? != identity
        {
            return collision_or_corruption(expected_hash);
        }
        // Check the path still names the held, verified file before publishing.
        let path_file = self.open_exact_owned_file(&canonical)?;
        if exact_file_identity(&path_file)? != identity {
            return exact_object_changed();
        }
        let mut directory_entries_synced = true;
        let assets = self.ensure_directory(
            &self.repository_root,
            "assets-v2",
            &mut directory_entries_synced,
        )?;
        let objects = self.ensure_directory(&assets, "objects", &mut directory_entries_synced)?;
        let shard =
            self.ensure_directory(&objects, &expected_hash[..2], &mut directory_entries_synced)?;
        let object_path = shard.join(&expected_hash[2..]);
        let physical_key = object_physical_key(expected_hash);
        if cancelled() {
            return Err(io::Error::other("import cancelled"));
        }
        #[cfg(any(target_os = "android", windows))]
        let published = crate::trust_boundary::rename_without_replace(&canonical, &object_path);
        #[cfg(not(any(target_os = "android", windows)))]
        let published = fs::hard_link(&canonical, &object_path);
        let deduplicated = match published {
            Ok(()) => false,
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                self.verify_existing_object(
                    &object_path,
                    expected_hash,
                    expected_size,
                    &physical_key,
                )?;
                true
            }
            Err(error) => return Err(error),
        };
        directory_entries_synced &= sync_directory(&shard)?;
        // Both held handles deny writes on Windows. Drop them before unlinking
        // the staging name; the CAS name is now durable and never overwritten.
        drop(path_file);
        drop(file);
        #[cfg(any(target_os = "android", windows))]
        if deduplicated {
            fs::remove_file(&canonical)?;
        }
        #[cfg(not(any(target_os = "android", windows)))]
        fs::remove_file(&canonical)?;
        directory_entries_synced &= sync_directory(canonical.parent().expect("payload parent"))?;
        Ok(PreparedPayload {
            content_hash: expected_hash.to_owned(),
            byte_size: expected_size,
            physical_key,
            deduplicated,
            directory_entries_synced,
        })
    }

    fn prepare_reader_inner(
        &self,
        reader: &mut impl Read,
        expected: Option<(&str, u64)>,
    ) -> Result<PreparedPayload, io::Error> {
        self.ensure_repository_root()?;
        let mut directory_entries_synced = true;
        let assets_directory = self.ensure_directory(
            &self.repository_root,
            "assets-v2",
            &mut directory_entries_synced,
        )?;
        let staging_directory =
            self.ensure_directory(&assets_directory, "staging", &mut directory_entries_synced)?;
        let staging_path = staging_directory.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let (mut file, mut staging) = create_staging_file(&staging_path, &staging_directory)?;
        let mut hasher = Sha256::new();
        let mut byte_size = 0_u64;
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            file.write_all(&buffer[..read])?;
            hasher.update(&buffer[..read]);
            byte_size = byte_size
                .checked_add(read as u64)
                .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "payload size overflow"))?;
        }
        file.flush()?;
        file.sync_all()?;
        drop(file);
        directory_entries_synced &= sync_directory(&staging_directory)?;

        let content_hash = hex::encode(hasher.finalize());
        if let Some((expected_content_hash, expected_byte_size)) = expected {
            if content_hash != expected_content_hash || byte_size != expected_byte_size {
                return Err(io::Error::new(
                    ErrorKind::InvalidData,
                    "payload does not match its expected content hash and size",
                ));
            }
        }
        let physical_key = object_physical_key(&content_hash);
        let objects_directory =
            self.ensure_directory(&assets_directory, "objects", &mut directory_entries_synced)?;
        let object_directory = self.ensure_directory(
            &objects_directory,
            &content_hash[..2],
            &mut directory_entries_synced,
        )?;
        let object_path = object_directory.join(&content_hash[2..]);

        #[cfg(target_os = "android")]
        let publication =
            crate::trust_boundary::rename_without_replace(&staging_path, &object_path);
        #[cfg(not(target_os = "android"))]
        let publication = fs::hard_link(&staging_path, &object_path);
        let deduplicated = match publication {
            Ok(()) => {
                directory_entries_synced &= sync_directory(&object_directory)?;
                false
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                self.verify_existing_object(&object_path, &content_hash, byte_size, &physical_key)?;
                directory_entries_synced &= sync_directory(&object_directory)?;
                true
            }
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("failed to publish CAS object without replacement: {error}"),
                ));
            }
        };

        directory_entries_synced &= staging.remove_and_sync()?;
        Ok(PreparedPayload {
            content_hash,
            byte_size,
            physical_key,
            deduplicated,
            directory_entries_synced,
        })
    }

    pub fn stat_object(&self, content_hash: &str) -> io::Result<Option<u64>> {
        let Some(path) = self.existing_object_path(content_hash)? else {
            return Ok(None);
        };
        Ok(Some(fs::symlink_metadata(path)?.len()))
    }

    pub fn read_object(&self, content_hash: &str) -> io::Result<Option<Vec<u8>>> {
        let Some(path) = self.existing_object_path(content_hash)? else {
            return Ok(None);
        };
        Ok(Some(fs::read(path)?))
    }

    pub fn read_object_range(
        &self,
        content_hash: &str,
        start: u64,
        end_exclusive: u64,
    ) -> io::Result<Option<Vec<u8>>> {
        if end_exclusive < start {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "payload range bounds must be in ascending order",
            ));
        }
        let Some(path) = self.existing_object_path(content_hash)? else {
            return Ok(None);
        };
        let mut file = File::open(path)?;
        let size = file.metadata()?.len();
        let bounded_start = start.min(size);
        let bounded_end = end_exclusive.min(size);
        let length = usize::try_from(bounded_end - bounded_start)
            .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "payload range is too large"))?;
        file.seek(SeekFrom::Start(bounded_start))?;
        let mut data = vec![0; length];
        file.read_exact(&mut data)?;
        Ok(Some(data))
    }

    pub fn open_object(&self, content_hash: &str) -> io::Result<Option<File>> {
        let Some(path) = self.existing_object_path(content_hash)? else {
            return Ok(None);
        };
        Ok(Some(File::open(path)?))
    }

    pub fn object_path(&self, content_hash: &str) -> io::Result<Option<PathBuf>> {
        self.existing_object_path(content_hash)
    }

    pub(crate) fn unlink_exact_object(
        &self,
        content_hash: &str,
        expected_byte_size: u64,
        expected_physical_key: &str,
    ) -> io::Result<ExactObjectUnlink> {
        self.unlink_exact_object_inner(
            content_hash,
            expected_byte_size,
            expected_physical_key,
            |_| Ok(()),
        )
    }

    #[cfg(test)]
    fn unlink_exact_object_with_hash_hook(
        &self,
        content_hash: &str,
        expected_byte_size: u64,
        expected_physical_key: &str,
        after_hash: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<ExactObjectUnlink> {
        self.unlink_exact_object_inner(
            content_hash,
            expected_byte_size,
            expected_physical_key,
            after_hash,
        )
    }

    fn unlink_exact_object_inner(
        &self,
        content_hash: &str,
        expected_byte_size: u64,
        expected_physical_key: &str,
        after_hash: impl FnOnce(&Path) -> io::Result<()>,
    ) -> io::Result<ExactObjectUnlink> {
        validate_content_hash(content_hash)?;
        let physical_key = object_physical_key(content_hash);
        if expected_physical_key != physical_key {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "deletion tombstone physical key is not the exact canonical object key",
            ));
        }
        let Some(path) = self.existing_object_path(content_hash)? else {
            return Ok(ExactObjectUnlink::Missing);
        };
        let mut file = self.open_exact_owned_file(&path)?;
        let identity = exact_file_identity(&file)?;
        if identity.byte_size() != expected_byte_size {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "deletion tombstone size does not match the exact canonical object",
            ));
        }
        let actual_hash = hash_open_file(&mut file)?;
        if exact_file_identity(&file)? != identity {
            return exact_object_changed();
        }
        if actual_hash != content_hash {
            return collision_or_corruption(expected_physical_key);
        }
        after_hash(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        self.validate_owned_file(&path, &metadata)?;
        let final_file = self.open_exact_owned_file(&path)?;
        if exact_file_identity(&final_file)? != identity {
            return exact_object_changed();
        }
        fs::remove_file(&path)?;
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                ErrorKind::InvalidData,
                "canonical object path has no containing directory",
            )
        })?;
        Ok(ExactObjectUnlink::Removed {
            directory_entries_synced: sync_directory(parent)?,
        })
    }

    fn ensure_repository_root(&self) -> io::Result<()> {
        reject_link_components(&self.repository_root)?;
        let metadata = fs::symlink_metadata(&self.repository_root)?;
        ensure_real_directory(&self.repository_root, &metadata)?;
        let canonical = fs::canonicalize(&self.repository_root)?;
        if canonical != self.repository_root {
            return invalid_owned_path(&self.repository_root, "repository root changed");
        }
        Ok(())
    }

    fn ensure_directory(
        &self,
        parent: &Path,
        name: &str,
        directory_entries_synced: &mut bool,
    ) -> io::Result<PathBuf> {
        self.ensure_directory_with_sync(parent, name, directory_entries_synced, sync_directory)
    }

    fn ensure_directory_with_sync(
        &self,
        parent: &Path,
        name: &str,
        directory_entries_synced: &mut bool,
        mut sync_parent: impl FnMut(&Path) -> io::Result<bool>,
    ) -> io::Result<PathBuf> {
        let path = parent.join(name);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => self.validate_owned_directory(&path, &metadata)?,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                match fs::create_dir(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                let metadata = fs::symlink_metadata(&path)?;
                self.validate_owned_directory(&path, &metadata)?;
            }
            Err(error) => return Err(error),
        }
        *directory_entries_synced &= sync_parent(parent)?;
        Ok(path)
    }

    fn existing_object_path(&self, content_hash: &str) -> io::Result<Option<PathBuf>> {
        validate_content_hash(content_hash)?;
        self.ensure_repository_root()?;
        let assets_directory = self.repository_root.join("assets-v2");
        if !self.existing_owned_directory(&assets_directory)? {
            return Ok(None);
        }
        let objects_directory = assets_directory.join("objects");
        if !self.existing_owned_directory(&objects_directory)? {
            return Ok(None);
        }
        let shard_directory = objects_directory.join(&content_hash[..2]);
        if !self.existing_owned_directory(&shard_directory)? {
            return Ok(None);
        }
        let object_path = shard_directory.join(&content_hash[2..]);
        match fs::symlink_metadata(&object_path) {
            Ok(metadata) => {
                self.validate_owned_file(&object_path, &metadata)?;
                Ok(Some(object_path))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn existing_owned_directory(&self, path: &Path) -> io::Result<bool> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                self.validate_owned_directory(path, &metadata)?;
                Ok(true)
            }
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn validate_owned_directory(&self, path: &Path, metadata: &Metadata) -> io::Result<()> {
        if is_link_like(metadata) {
            return invalid_owned_path(path, "linked directory is forbidden");
        }
        ensure_real_directory(path, metadata)?;
        self.ensure_canonical_confinement(path)
    }

    fn validate_owned_file(&self, path: &Path, metadata: &Metadata) -> io::Result<()> {
        if is_link_like(metadata) {
            return invalid_owned_path(path, "linked object is forbidden");
        }
        if !metadata.is_file() {
            return invalid_owned_path(path, "content-addressed object is not a file");
        }
        self.ensure_canonical_confinement(path)
    }

    fn open_exact_owned_file(&self, path: &Path) -> io::Result<File> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        #[cfg(windows)]
        {
            use windows_sys::Win32::Storage::FileSystem::{
                FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ,
            };
            options
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE);
        }
        let file = options.open(path)?;
        let metadata = file.metadata()?;
        self.validate_owned_file(path, &metadata)?;
        Ok(file)
    }

    fn ensure_canonical_confinement(&self, path: &Path) -> io::Result<()> {
        let canonical = fs::canonicalize(path)?;
        if !canonical.starts_with(&self.repository_root) {
            return invalid_owned_path(path, "path escapes the repository root");
        }
        Ok(())
    }

    fn verify_existing_object(
        &self,
        path: &Path,
        expected_hash: &str,
        expected_size: u64,
        physical_key: &str,
    ) -> io::Result<()> {
        let mut file = self.open_exact_owned_file(path)?;
        if file.metadata()?.len() != expected_size {
            return collision_or_corruption(physical_key);
        }
        if hash_open_file(&mut file)? != expected_hash {
            return collision_or_corruption(physical_key);
        }
        Ok(())
    }
}

fn create_staging_file(path: &Path, parent: &Path) -> io::Result<(File, StagingFile)> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let staging = StagingFile {
        path: path.to_path_buf(),
        parent: parent.to_path_buf(),
        owned: true,
    };
    Ok((file, staging))
}

fn absolute_path(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn reject_link_components(path: &Path) -> io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::CurDir
        ) {
            continue;
        }
        let metadata = fs::symlink_metadata(&current)?;
        if is_link_like(&metadata) {
            return invalid_owned_path(&current, "linked path component is forbidden");
        }
    }
    Ok(())
}

fn ensure_real_directory(path: &Path, metadata: &Metadata) -> io::Result<()> {
    if is_link_like(metadata) {
        return invalid_owned_path(path, "linked directory is forbidden");
    }
    if !metadata.is_dir() {
        return invalid_owned_path(path, "repository path is not a directory");
    }
    Ok(())
}

fn invalid_owned_path<T>(path: &Path, reason: &str) -> io::Result<T> {
    Err(io::Error::new(
        ErrorKind::InvalidData,
        format!("{reason}: {}", path.display()),
    ))
}

fn validate_content_hash(content_hash: &str) -> io::Result<()> {
    if is_lower_hex_256(content_hash) {
        return Ok(());
    }
    Err(io::Error::new(
        ErrorKind::InvalidInput,
        "content hash must be 64 lowercase hexadecimal characters",
    ))
}

pub(crate) fn object_physical_key(content_hash: &str) -> String {
    format!(
        "assets-v2/objects/{}/{}",
        &content_hash[..2],
        &content_hash[2..]
    )
}

fn collision_or_corruption<T>(physical_key: &str) -> io::Result<T> {
    Err(io::Error::new(
        ErrorKind::InvalidData,
        format!("payload collision or corruption at {physical_key}"),
    ))
}

fn hash_open_file(file: &mut File) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn exact_object_changed<T>() -> io::Result<T> {
    Err(io::Error::new(
        ErrorKind::InvalidData,
        "exact canonical object changed while it was being verified for deletion",
    ))
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExactFileIdentity {
    device: u64,
    inode: u64,
    byte_size: u64,
    mode: u32,
    links: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[cfg(unix)]
impl ExactFileIdentity {
    fn byte_size(self) -> u64 {
        self.byte_size
    }
}

#[cfg(unix)]
pub(crate) fn exact_file_identity(file: &File) -> io::Result<ExactFileIdentity> {
    let metadata = file.metadata()?;
    Ok(ExactFileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        byte_size: metadata.len(),
        mode: metadata.mode(),
        links: metadata.nlink(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
    })
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExactFileIdentity {
    volume: u32,
    file_index: u64,
    byte_size: u64,
    links: u32,
    attributes: u32,
    creation_time: u64,
    modified_time: u64,
}

#[cfg(windows)]
impl ExactFileIdentity {
    fn byte_size(self) -> u64 {
        self.byte_size
    }
}

#[cfg(windows)]
pub(crate) fn exact_file_identity(file: &File) -> io::Result<ExactFileIdentity> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let succeeded =
        unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) };
    if succeeded == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(ExactFileIdentity {
        volume: information.dwVolumeSerialNumber,
        file_index: (u64::from(information.nFileIndexHigh) << 32)
            | u64::from(information.nFileIndexLow),
        byte_size: (u64::from(information.nFileSizeHigh) << 32)
            | u64::from(information.nFileSizeLow),
        links: information.nNumberOfLinks,
        attributes: information.dwFileAttributes,
        creation_time: (u64::from(information.ftCreationTime.dwHighDateTime) << 32)
            | u64::from(information.ftCreationTime.dwLowDateTime),
        modified_time: (u64::from(information.ftLastWriteTime.dwHighDateTime) << 32)
            | u64::from(information.ftLastWriteTime.dwLowDateTime),
    })
}

#[cfg(not(any(unix, windows)))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExactFileIdentity {
    byte_size: u64,
    modified: Option<std::time::SystemTime>,
}

#[cfg(not(any(unix, windows)))]
impl ExactFileIdentity {
    fn byte_size(self) -> u64 {
        self.byte_size
    }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn exact_file_identity(file: &File) -> io::Result<ExactFileIdentity> {
    let metadata = file.metadata()?;
    Ok(ExactFileIdentity {
        byte_size: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::{create_staging_file, ExactObjectUnlink, PayloadCas};
    use sha2::Digest;
    use std::io::Cursor;

    #[test]
    fn duplicate_publication_verifies_existing_bytes_and_cleans_staging() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let original = b"original synthetic object";
        let prepared = cas.prepare_bytes(original).unwrap();
        assert!(!prepared.deduplicated);
        assert!(cas.prepare_bytes(original).unwrap().deduplicated);
        let object = directory.path().join(&prepared.physical_key);
        let corrupted = vec![b'x'; original.len()];
        std::fs::write(&object, &corrupted).unwrap();
        let error = cas.prepare_bytes(original).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error
            .to_string()
            .contains("payload collision or corruption"));
        assert_eq!(std::fs::read(object).unwrap(), corrupted);
        assert_eq!(
            std::fs::read_dir(directory.path().join("assets-v2/staging"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn concurrent_publication_never_replaces_the_winning_object() {
        let directory = tempfile::tempdir().unwrap();
        let cas = std::sync::Arc::new(PayloadCas::new(directory.path()).unwrap());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let cas = cas.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    cas.prepare_bytes(b"concurrent synthetic object").unwrap()
                })
            })
            .collect();
        let published: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            published
                .iter()
                .filter(|object| !object.deduplicated)
                .count(),
            1
        );
        assert_eq!(
            cas.read_object(&published[0].content_hash)
                .unwrap()
                .unwrap(),
            b"concurrent synthetic object"
        );
        assert_eq!(
            std::fs::read_dir(directory.path().join("assets-v2/staging"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn existing_directory_from_a_crash_window_resyncs_its_parent_before_acceptance() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let cas = PayloadCas::new(directory.path()).expect("open repository");
        let existing = directory.path().join("assets-v2");
        std::fs::create_dir(&existing).expect("simulate concurrent directory creation");
        let mut directory_entries_synced = true;
        let mut synced_parent = None;

        let accepted = cas
            .ensure_directory_with_sync(
                &cas.repository_root,
                "assets-v2",
                &mut directory_entries_synced,
                |parent| {
                    synced_parent = Some(parent.to_path_buf());
                    Ok(false)
                },
            )
            .expect("accept existing directory");

        assert_eq!(accepted, cas.repository_root.join("assets-v2"));
        assert_eq!(
            synced_parent.as_deref(),
            Some(cas.repository_root.as_path())
        );
        assert!(!directory_entries_synced);
    }

    #[test]
    fn failed_create_new_does_not_own_or_remove_a_preexisting_staging_file() {
        let directory = tempfile::tempdir().expect("temporary staging directory");
        let staging_path = directory.path().join("collision.tmp");
        std::fs::write(&staging_path, b"preexisting").expect("seed staging collision");

        let error = match create_staging_file(&staging_path, directory.path()) {
            Ok(_) => panic!("staging collision must fail"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(staging_path).expect("preexisting staging file remains"),
            b"preexisting"
        );
    }

    #[test]
    fn expected_prepare_rejects_mismatch_before_canonical_publication() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let cas = PayloadCas::new(directory.path()).expect("open repository");
        let expected = b"expected payload";
        let wrong = b"unexpected data";
        let expected_hash = hex::encode(sha2::Sha256::digest(expected));
        let wrong_hash = hex::encode(sha2::Sha256::digest(wrong));

        for _ in 0..3 {
            let error = cas
                .prepare_reader_expected(
                    &mut Cursor::new(wrong),
                    &expected_hash,
                    expected.len() as u64,
                )
                .expect_err("wrong payload must be rejected");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert_eq!(cas.stat_object(&wrong_hash).unwrap(), None);
            assert_eq!(
                std::fs::read_dir(directory.path().join("assets-v2").join("staging"))
                    .unwrap()
                    .count(),
                0
            );
        }

        let prepared = cas
            .prepare_reader_expected(
                &mut Cursor::new(expected),
                &expected_hash,
                expected.len() as u64,
            )
            .expect("exact payload is published");
        assert_eq!(prepared.content_hash, expected_hash);
        assert_eq!(prepared.byte_size, expected.len() as u64);

        assert!(cas
            .prepare_reader_expected(
                &mut Cursor::new(wrong),
                &expected_hash,
                expected.len() as u64,
            )
            .is_err());
        assert_eq!(cas.read_object(&expected_hash).unwrap().unwrap(), expected);
        assert_eq!(cas.stat_object(&wrong_hash).unwrap(), None);
    }

    #[test]
    fn exact_object_unlink_rejects_path_and_size_mismatch_without_touching_bytes() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let cas = PayloadCas::new(directory.path()).expect("open repository");
        let prepared = cas.prepare_bytes(b"exact deletion candidate").unwrap();

        let wrong_path = cas
            .unlink_exact_object(
                &prepared.content_hash,
                prepared.byte_size,
                "assets-v2/objects/00/not-the-canonical-object",
            )
            .expect_err("mismatched physical key must fail closed");
        assert_eq!(wrong_path.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            cas.read_object(&prepared.content_hash).unwrap(),
            Some(b"exact deletion candidate".to_vec())
        );

        let wrong_size = cas
            .unlink_exact_object(
                &prepared.content_hash,
                prepared.byte_size + 1,
                &prepared.physical_key,
            )
            .expect_err("mismatched byte size must fail closed");
        assert_eq!(wrong_size.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(
            cas.stat_object(&prepared.content_hash).unwrap(),
            Some(prepared.byte_size)
        );
    }

    #[test]
    fn exact_object_unlink_removes_only_the_canonical_regular_file() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let cas = PayloadCas::new(directory.path()).expect("open repository");
        let prepared = cas.prepare_bytes(b"unlink me exactly").unwrap();

        let outcome = cas
            .unlink_exact_object(
                &prepared.content_hash,
                prepared.byte_size,
                &prepared.physical_key,
            )
            .expect("unlink exact object");

        assert!(matches!(
            outcome,
            ExactObjectUnlink::Removed {
                directory_entries_synced: _
            }
        ));
        assert_eq!(cas.stat_object(&prepared.content_hash).unwrap(), None);
        assert_eq!(
            cas.unlink_exact_object(
                &prepared.content_hash,
                prepared.byte_size,
                &prepared.physical_key,
            )
            .unwrap(),
            ExactObjectUnlink::Missing
        );
    }

    #[test]
    fn exact_object_unlink_rejects_a_symlink_or_reparse_object() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let outside = tempfile::tempdir().expect("temporary outside directory");
        let cas = PayloadCas::new(directory.path()).expect("open repository");
        let prepared = cas.prepare_bytes(b"linked deletion candidate").unwrap();
        let object_path = directory.path().join(&prepared.physical_key);
        let target = outside.path().join("target.bin");
        std::fs::write(&target, b"outside bytes").unwrap();
        std::fs::remove_file(&object_path).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &object_path).unwrap();
        #[cfg(windows)]
        if let Err(error) = std::os::windows::fs::symlink_file(&target, &object_path) {
            if error.kind() == std::io::ErrorKind::PermissionDenied {
                return;
            }
            panic!("create reparse fixture: {error}");
        }

        let error = cas
            .unlink_exact_object(
                &prepared.content_hash,
                prepared.byte_size,
                &prepared.physical_key,
            )
            .expect_err("linked object must fail closed");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(&target).unwrap(), b"outside bytes");
        assert!(std::fs::symlink_metadata(object_path)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn exact_object_unlink_rejects_same_size_replacement_after_streamed_hashing() {
        let directory = tempfile::tempdir().expect("temporary repository");
        let cas = PayloadCas::new(directory.path()).expect("open repository");
        let original = vec![0x51; super::COPY_BUFFER_BYTES * 3 + 17];
        let replacement = vec![0x72; original.len()];
        let prepared = cas.prepare_bytes(&original).unwrap();
        let object_path = directory.path().join(&prepared.physical_key);

        let error = cas
            .unlink_exact_object_with_hash_hook(
                &prepared.content_hash,
                prepared.byte_size,
                &prepared.physical_key,
                |_| {
                    std::fs::remove_file(&object_path)?;
                    std::fs::write(&object_path, &replacement)
                },
            )
            .expect_err("same-size replacement after hashing must fail closed");

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(std::fs::read(object_path).unwrap(), replacement);
    }
}
