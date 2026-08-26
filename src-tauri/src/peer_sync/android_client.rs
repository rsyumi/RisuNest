use super::{
    DownloadReport, LanCloneClient, LoopbackCloneClient, PeerSyncError, TransferCancellation,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

const JOB_SCHEMA: &str = "risunest.android-peer-clone-job/v1";
const JOB_OWNERSHIP_SCHEMA: &str = "risunest.android-peer-clone-ownership/v1";
const VERIFIED_SCHEMA: &str = "risunest.android-peer-clone-verified/v1";
const CANCEL_REQUESTED_SCHEMA: &str = "risunest.android-peer-clone-cancel/v1";
const MAX_JOB_RECORD_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AndroidCloneJobPhase {
    Ready,
    VerifiedAwaitingActivation,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AndroidCloneJobDescriptor {
    schema: String,
    job_id: String,
    manifest_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AndroidCloneJobOwnership {
    schema: String,
    job_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AndroidCloneJobMarker {
    schema: String,
    manifest_id: String,
}

pub struct AndroidResumableCloneJob {
    root: PathBuf,
    descriptor: AndroidCloneJobDescriptor,
    client: LoopbackCloneClient,
}

impl AndroidResumableCloneJob {
    pub fn claim(
        job_root: impl AsRef<Path>,
        endpoint: &str,
        session_id: &str,
        manifest_id: &str,
        claim: &str,
    ) -> Result<Self, PeerSyncError> {
        let job_root = job_root.as_ref();
        let job_id = validate_job_id_from_path(job_root)?;
        fs::create_dir(job_root)?;
        let root = fs::canonicalize(job_root)?;
        let result = (|| {
            write_new_json(
                &root.join("ownership.json"),
                &AndroidCloneJobOwnership {
                    schema: JOB_OWNERSHIP_SCHEMA.to_owned(),
                    job_id: job_id.clone(),
                },
            )?;
            let descriptor = AndroidCloneJobDescriptor {
                schema: JOB_SCHEMA.to_owned(),
                job_id: job_id.clone(),
                manifest_id: manifest_id.to_owned(),
            };
            write_new_json(&root.join("job.json"), &descriptor)?;
            let lan = LanCloneClient::claim_and_persist(
                &root.join("credential.json"),
                endpoint,
                session_id,
                manifest_id,
                claim,
            )?;
            let client = LoopbackCloneClient::from_lan(&root, lan, manifest_id)?;
            Ok(Self {
                root: root.clone(),
                descriptor,
                client,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&root);
        }
        result
    }

    pub fn open(job_root: impl AsRef<Path>) -> Result<Self, PeerSyncError> {
        let (root, descriptor) = validate_job(job_root.as_ref())?;
        validate_marker_if_present(
            &root.join("verified.json"),
            VERIFIED_SCHEMA,
            &descriptor.manifest_id,
        )?;
        validate_marker_if_present(
            &root.join("cancel.requested"),
            CANCEL_REQUESTED_SCHEMA,
            &descriptor.manifest_id,
        )?;
        let lan = LanCloneClient::open_persisted(&root.join("credential.json"))?;
        let client = LoopbackCloneClient::from_lan(&root, lan, &descriptor.manifest_id)?;
        Ok(Self {
            root,
            descriptor,
            client,
        })
    }

    pub fn phase(&self) -> Result<AndroidCloneJobPhase, PeerSyncError> {
        if self.root.join("verified.json").is_file() {
            validate_marker_if_present(
                &self.root.join("verified.json"),
                VERIFIED_SCHEMA,
                &self.descriptor.manifest_id,
            )?;
            Ok(AndroidCloneJobPhase::VerifiedAwaitingActivation)
        } else {
            Ok(AndroidCloneJobPhase::Ready)
        }
    }

    pub fn download(
        &mut self,
        cancellation: &TransferCancellation,
    ) -> Result<DownloadReport, PeerSyncError> {
        self.download_with_progress(cancellation, |_| {})
    }

    pub fn download_with_progress(
        &mut self,
        cancellation: &TransferCancellation,
        progress: impl FnMut(u64),
    ) -> Result<DownloadReport, PeerSyncError> {
        if self.cancel_requested()? {
            return Err(PeerSyncError::Cancelled);
        }
        let report = self.client.download_with_progress(cancellation, progress)?;
        if !self.root.join("verified.json").exists() {
            write_new_json(
                &self.root.join("verified.json"),
                &AndroidCloneJobMarker {
                    schema: VERIFIED_SCHEMA.to_owned(),
                    manifest_id: self.descriptor.manifest_id.clone(),
                },
            )?;
        }
        Ok(report)
    }

    pub fn request_cancel(&self) -> Result<(), PeerSyncError> {
        write_cancel_marker(&self.root, &self.descriptor.manifest_id)
    }

    pub fn request_cancel_at(job_root: impl AsRef<Path>) -> Result<(), PeerSyncError> {
        let (root, descriptor) = validate_job(job_root.as_ref())?;
        write_cancel_marker(&root, &descriptor.manifest_id)
    }

    pub fn discard_at(job_root: impl AsRef<Path>) -> Result<(), PeerSyncError> {
        let job_root = job_root.as_ref();
        if !job_root.exists() {
            return Ok(());
        }
        let (root, descriptor) = validate_job(job_root)?;
        cleanup_owned_job(&root, &descriptor.job_id)
    }

    pub fn discard(self) -> Result<(), PeerSyncError> {
        cleanup_owned_job(&self.root, &self.descriptor.job_id)
    }
}

fn write_cancel_marker(root: &Path, manifest_id: &str) -> Result<(), PeerSyncError> {
    let path = root.join("cancel.requested");
    if path.exists() {
        return validate_marker_if_present(&path, CANCEL_REQUESTED_SCHEMA, manifest_id);
    }
    write_new_json(
        &path,
        &AndroidCloneJobMarker {
            schema: CANCEL_REQUESTED_SCHEMA.to_owned(),
            manifest_id: manifest_id.to_owned(),
        },
    )
}

impl AndroidResumableCloneJob {
    pub fn cancel_requested(&self) -> Result<bool, PeerSyncError> {
        let path = self.root.join("cancel.requested");
        if !path.exists() {
            return Ok(false);
        }
        validate_marker_if_present(&path, CANCEL_REQUESTED_SCHEMA, &self.descriptor.manifest_id)?;
        Ok(true)
    }
}

fn validate_job(job_root: &Path) -> Result<(PathBuf, AndroidCloneJobDescriptor), PeerSyncError> {
    let job_id = validate_job_id_from_path(job_root)?;
    let root = fs::canonicalize(job_root)?;
    validate_ownership(&root, &job_id)?;
    let descriptor: AndroidCloneJobDescriptor =
        read_bounded_json(&root.join("job.json"), MAX_JOB_RECORD_BYTES)?;
    if descriptor.schema != JOB_SCHEMA
        || descriptor.job_id != job_id
        || !is_sha256(&descriptor.manifest_id)
    {
        return Err(PeerSyncError::Storage(
            "Android clone job descriptor is invalid".to_owned(),
        ));
    }
    Ok((root, descriptor))
}

fn validate_job_id_from_path(path: &Path) -> Result<String, PeerSyncError> {
    let value = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            PeerSyncError::Storage("Android clone job path has no UTF-8 identity".to_owned())
        })?;
    let parsed = uuid::Uuid::parse_str(value)
        .map_err(|_| PeerSyncError::Storage("Android clone job identity is invalid".to_owned()))?;
    if parsed.get_version_num() != 4 || parsed.to_string() != value {
        return Err(PeerSyncError::Storage(
            "Android clone job identity must be a canonical UUID v4".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn validate_ownership(root: &Path, expected_job_id: &str) -> Result<(), PeerSyncError> {
    if root.file_name().and_then(|value| value.to_str()) != Some(expected_job_id) {
        return Err(PeerSyncError::Storage(
            "Android clone job root changed identity".to_owned(),
        ));
    }
    let ownership: AndroidCloneJobOwnership =
        read_bounded_json(&root.join("ownership.json"), MAX_JOB_RECORD_BYTES)?;
    if ownership.schema != JOB_OWNERSHIP_SCHEMA || ownership.job_id != expected_job_id {
        return Err(PeerSyncError::Storage(
            "Android clone job ownership is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn validate_marker_if_present(
    path: &Path,
    schema: &str,
    manifest_id: &str,
) -> Result<(), PeerSyncError> {
    if !path.exists() {
        return Ok(());
    }
    let marker: AndroidCloneJobMarker = read_bounded_json(path, MAX_JOB_RECORD_BYTES)?;
    if marker.schema != schema || marker.manifest_id != manifest_id {
        return Err(PeerSyncError::Storage(
            "Android clone job marker is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn write_new_json(path: &Path, value: &impl Serialize) -> Result<(), PeerSyncError> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    if bytes.len() as u64 > MAX_JOB_RECORD_BYTES {
        return Err(PeerSyncError::Storage(
            "Android clone job record exceeds its bound".to_owned(),
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        PeerSyncError::Storage("Android clone job record has no parent".to_owned())
    })?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| {
            PeerSyncError::Storage("Android clone job record has no UTF-8 name".to_owned())
        })?;
    let temporary = parent.join(format!(".{file_name}-{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        match fs::rename(&temporary, path) {
            Ok(()) => sync_parent_directory(parent),
            Err(error) => {
                let existing = fs::symlink_metadata(path)
                    .ok()
                    .filter(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
                    .and_then(|_| fs::read(path).ok());
                if existing.as_deref() == Some(bytes.as_slice()) {
                    Ok(())
                } else {
                    Err(error.into())
                }
            }
        }
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(
    path: &Path,
    maximum: u64,
) -> Result<T, PeerSyncError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > maximum {
        return Err(PeerSyncError::Storage(
            "invalid Android clone job record".to_owned(),
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(maximum + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > maximum {
        return Err(PeerSyncError::Storage(
            "Android clone job record exceeds its bound".to_owned(),
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| PeerSyncError::Storage(error.to_string()))
}

fn cleanup_owned_job(root: &Path, expected_job_id: &str) -> Result<(), PeerSyncError> {
    let canonical = match fs::canonicalize(root) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    validate_ownership(&canonical, expected_job_id)?;
    fs::remove_dir_all(canonical)?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), PeerSyncError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> Result<(), PeerSyncError> {
    Ok(())
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
