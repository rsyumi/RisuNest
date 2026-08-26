use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
static PUBLISH_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, PartialEq, Eq)]
pub struct PreparedPayload {
    pub content_hash: String,
    pub byte_size: u64,
    pub physical_key: String,
    pub deduplicated: bool,
}

pub struct PayloadCas {
    repository_root: PathBuf,
}

struct StagingFile {
    path: PathBuf,
    published: bool,
}

impl Drop for StagingFile {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl PayloadCas {
    pub fn new(repository_root: impl AsRef<Path>) -> Self {
        Self {
            repository_root: repository_root.as_ref().to_path_buf(),
        }
    }

    pub fn prepare_bytes(&self, data: &[u8]) -> Result<PreparedPayload, io::Error> {
        self.prepare_reader(&mut io::Cursor::new(data))
    }

    pub fn prepare_reader(&self, reader: &mut impl Read) -> Result<PreparedPayload, io::Error> {
        let staging_directory = self.repository_root.join("assets-v2").join("staging");
        fs::create_dir_all(&staging_directory)?;
        let staging_path = staging_directory.join(format!("{}.tmp", uuid::Uuid::new_v4()));
        let mut staging = StagingFile {
            path: staging_path.clone(),
            published: false,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staging_path)?;
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

        let content_hash = hex::encode(hasher.finalize());
        let physical_key = object_physical_key(&content_hash);
        let object_path = self.repository_root.join(&physical_key);
        let object_directory = object_path
            .parent()
            .expect("content-addressed object path has a parent");
        fs::create_dir_all(object_directory)?;

        let _publish = PUBLISH_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let deduplicated = if object_path.exists() {
            verify_existing_object(&object_path, &content_hash, byte_size, &physical_key)?;
            true
        } else {
            match fs::rename(&staging_path, &object_path) {
                Ok(()) => {
                    staging.published = true;
                    sync_directory(object_directory)?;
                    false
                }
                Err(_) if object_path.exists() => {
                    verify_existing_object(&object_path, &content_hash, byte_size, &physical_key)?;
                    true
                }
                Err(error) => return Err(error),
            }
        };

        Ok(PreparedPayload {
            content_hash,
            byte_size,
            physical_key,
            deduplicated,
        })
    }

    pub fn stat_object(&self, content_hash: &str) -> io::Result<Option<u64>> {
        let path = self.object_path(content_hash)?;
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_file() => Ok(Some(metadata.len())),
            Ok(_) => Err(io::Error::new(
                ErrorKind::InvalidData,
                "content-addressed object is not a file",
            )),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn read_object(&self, content_hash: &str) -> io::Result<Option<Vec<u8>>> {
        let path = self.object_path(content_hash)?;
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn object_path(&self, content_hash: &str) -> io::Result<PathBuf> {
        validate_content_hash(content_hash)?;
        Ok(self.repository_root.join(object_physical_key(content_hash)))
    }
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

fn object_physical_key(content_hash: &str) -> String {
    format!(
        "assets-v2/objects/{}/{}",
        &content_hash[..2],
        &content_hash[2..]
    )
}

fn verify_existing_object(
    path: &Path,
    expected_hash: &str,
    expected_size: u64,
    physical_key: &str,
) -> io::Result<()> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() != expected_size {
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

fn collision_or_corruption<T>(physical_key: &str) -> io::Result<T> {
    Err(io::Error::new(
        ErrorKind::InvalidData,
        format!("payload collision or corruption at {physical_key}"),
    ))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}
