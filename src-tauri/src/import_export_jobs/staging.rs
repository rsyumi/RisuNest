use super::FormatError;
use crate::trust_boundary::is_link_like;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, Metadata, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StagedPayload {
    pub staged_name: String,
    pub sha256: String,
    pub byte_size: u64,
}

#[derive(Debug)]
pub struct JobStaging {
    root: PathBuf,
}

struct PartialFile {
    path: PathBuf,
    owned: bool,
}

impl Drop for PartialFile {
    fn drop(&mut self) {
        if self.owned {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl JobStaging {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, FormatError> {
        let root = root.as_ref();
        let metadata = fs::symlink_metadata(root)
            .map_err(|error| FormatError::io("inspect job staging root", error))?;
        ensure_plain_directory(root, &metadata)?;
        let root = fs::canonicalize(root)
            .map_err(|error| FormatError::io("canonicalize job staging root", error))?;
        Ok(Self { root })
    }

    pub(crate) fn stage_reader(
        &self,
        reader: &mut impl Read,
        max_bytes: u64,
        cancelled: &impl Fn() -> bool,
    ) -> Result<StagedPayload, FormatError> {
        self.validate_root()?;
        if cancelled() {
            return Err(FormatError::cancelled());
        }

        let staged_name = format!("{}.payload", uuid::Uuid::new_v4());
        let path = self.root.join(&staged_name);
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| FormatError::io("create staged payload", error))?;
        let mut partial = PartialFile { path, owned: true };
        let mut buffer = [0_u8; COPY_BUFFER_BYTES];
        let mut hasher = Sha256::new();
        let mut byte_size = 0_u64;

        loop {
            if cancelled() {
                return Err(FormatError::cancelled());
            }
            let read = reader.read(&mut buffer).map_err(|error| {
                if error.kind() == std::io::ErrorKind::InvalidData {
                    FormatError::invalid("staged payload source is invalid")
                } else {
                    FormatError::io("read staged payload source", error)
                }
            })?;
            if read == 0 {
                break;
            }
            byte_size = byte_size
                .checked_add(read as u64)
                .ok_or_else(|| FormatError::limit("staged payload length overflow"))?;
            if byte_size > max_bytes {
                return Err(FormatError::limit("staged payload exceeds its byte limit"));
            }
            output
                .write_all(&buffer[..read])
                .map_err(|error| FormatError::io("write staged payload", error))?;
            hasher.update(&buffer[..read]);
        }

        if cancelled() {
            return Err(FormatError::cancelled());
        }
        output
            .flush()
            .map_err(|error| FormatError::io("flush staged payload", error))?;
        output
            .sync_all()
            .map_err(|error| FormatError::io("sync staged payload", error))?;
        drop(output);
        partial.owned = false;

        Ok(StagedPayload {
            staged_name,
            sha256: hex::encode(hasher.finalize()),
            byte_size,
        })
    }

    pub(crate) fn remove(&self, staged_name: &str) {
        if is_direct_staged_name(staged_name) {
            let _ = fs::remove_file(self.root.join(staged_name));
        }
    }

    fn validate_root(&self) -> Result<(), FormatError> {
        let metadata = fs::symlink_metadata(&self.root)
            .map_err(|error| FormatError::io("inspect job staging root", error))?;
        ensure_plain_directory(&self.root, &metadata)?;
        let canonical = fs::canonicalize(&self.root)
            .map_err(|error| FormatError::io("canonicalize job staging root", error))?;
        if canonical != self.root {
            return Err(FormatError::invalid("job staging root changed"));
        }
        Ok(())
    }
}

pub(crate) struct CreatedPayloads<'a> {
    staging: &'a JobStaging,
    names: Vec<String>,
    armed: bool,
}

impl<'a> CreatedPayloads<'a> {
    pub(crate) fn new(staging: &'a JobStaging) -> Self {
        Self {
            staging,
            names: Vec::new(),
            armed: true,
        }
    }

    pub(crate) fn track(&mut self, payload: &StagedPayload) {
        self.names.push(payload.staged_name.clone());
    }

    pub(crate) fn commit(mut self) {
        self.armed = false;
    }
}

impl Drop for CreatedPayloads<'_> {
    fn drop(&mut self) {
        if self.armed {
            for name in &self.names {
                self.staging.remove(name);
            }
        }
    }
}

fn is_direct_staged_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains('/')
        && !name.contains('\\')
        && Path::new(name).file_name().and_then(|part| part.to_str()) == Some(name)
}

fn ensure_plain_directory(path: &Path, metadata: &Metadata) -> Result<(), FormatError> {
    if is_link_like(metadata) || !metadata.is_dir() {
        return Err(FormatError::invalid(format!(
            "job staging root is not a plain directory: {}",
            path.display()
        )));
    }
    Ok(())
}
