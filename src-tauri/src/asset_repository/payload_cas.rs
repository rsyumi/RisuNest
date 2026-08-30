use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, Metadata, OpenOptions},
    io::{self, ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

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

        let deduplicated = match fs::hard_link(&staging_path, &object_path) {
            Ok(()) => {
                directory_entries_synced &= sync_directory(&object_directory)?;
                false
            }
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                self.verify_existing_object(&object_path, &content_hash, byte_size, &physical_key)?;
                directory_entries_synced &= sync_directory(&object_directory)?;
                true
            }
            Err(error) => return Err(error),
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
        let metadata = fs::symlink_metadata(&path)?;
        self.validate_owned_file(&path, &metadata)?;
        if metadata.len() != expected_byte_size {
            return Err(io::Error::new(
                ErrorKind::InvalidData,
                "deletion tombstone size does not match the exact canonical object",
            ));
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
        let metadata = fs::symlink_metadata(path)?;
        self.validate_owned_file(path, &metadata)?;
        if metadata.len() != expected_size {
            return collision_or_corruption(physical_key);
        }
        let mut file = File::open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        if hex::encode(hasher.finalize()) != expected_hash {
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

#[cfg(windows)]
fn is_link_like(metadata: &Metadata) -> bool {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_link_like(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn invalid_owned_path<T>(path: &Path, reason: &str) -> io::Result<T> {
    Err(io::Error::new(
        ErrorKind::InvalidData,
        format!("{reason}: {}", path.display()),
    ))
}

fn validate_content_hash(content_hash: &str) -> io::Result<()> {
    if content_hash.len() == 64
        && content_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
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

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<bool> {
    File::open(path)?.sync_all()?;
    Ok(true)
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::{create_staging_file, ExactObjectUnlink, PayloadCas};
    use sha2::Digest;
    use std::io::Cursor;

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
}
