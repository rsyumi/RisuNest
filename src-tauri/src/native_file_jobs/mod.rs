pub mod charx;
mod content;
pub mod screenshot_output;

mod lossless;
mod official_snapshot;

#[cfg(test)]
mod screenshot_output_test;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

const MAX_WARNING_CODES: usize = 16;
const MAX_CODE_BYTES: usize = 64;
const MAX_ERROR_MESSAGE_BYTES: usize = 512;
const MAX_MANIFEST_BYTES: u64 = 4096;
const MAX_CONCURRENT_JOBS: usize = 2;
const MAX_CLEANUP_ERRORS: usize = 4;
const ANDROID_SPOOL_FORMAT: &str = "risunest-android-saf-spool";
const ANDROID_SPOOL_VERSION: u8 = 1;
const ANDROID_SPOOL_STALE_MILLIS: u64 = 24 * 60 * 60 * 1_000;
const ANDROID_SPOOL_STAGING_PREFIX: &str = ".spooling-";
const ANDROID_SPOOL_CLEANUP_PREFIX: &str = ".cleanup-";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeJobError {
    pub(crate) code: String,
    pub(crate) message: String,
}

impl NativeJobError {
    pub(crate) fn new(code: &str, message: impl AsRef<str>) -> Self {
        Self {
            code: code.to_owned(),
            message: bounded_message(message.as_ref()),
        }
    }
}

impl std::fmt::Display for NativeJobError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for NativeJobError {}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum JobSource {
    DesktopPath { path: String },
    AndroidSpool { token: String },
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum SpoolState {
    Copying,
    Ready,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpoolManifest {
    token: String,
    state: SpoolState,
    display_name: String,
    bytes: Option<u64>,
    #[serde(default)]
    total_bytes: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SpoolOwnership {
    format: String,
    version: u8,
    token: String,
    created_at_millis: u64,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobOwnership {
    job_id: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum NativeFileJobStartRequest {
    RestoreBlockRisuSave {
        source: JobSource,
        expected_revision: i64,
    },
    RestoreLosslessBackup {
        source: JobSource,
        expected_revision: i64,
    },
    RestoreOfficialAccountSnapshot {
        base_url: String,
        credential: official_snapshot::OfficialSnapshotCredential,
        expected_revision: i64,
    },
    ExportBlockRisuSave {
        destination: String,
        expected_revision: i64,
        #[serde(default)]
        omit_account: bool,
    },
    ExportLosslessBackup {
        destination: Option<String>,
        expected_revision: i64,
    },
    PrepareContentImport {
        source: JobSource,
        display_name: String,
    },
    KeiBackupUpload {
        lease: String,
        expected_revision: i64,
        url: String,
        expected_account_id: String,
        token: String,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeFileJobStarted {
    pub(crate) job_id: String,
    pub(crate) warning_codes: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PreparedContentFormat {
    JsonCard,
    PngCard,
    CharxCard,
    AppendedCharxJpeg,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreparedContentAsset {
    pub(crate) reference_key: String,
    pub(crate) token: String,
    pub(crate) logical_id: String,
    pub(crate) object_hash: String,
    pub(crate) byte_size: u64,
    pub(crate) mime: String,
    pub(crate) name: String,
    pub(crate) ext: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreparedContentModule {
    pub(crate) trigger: Vec<Value>,
    pub(crate) regex: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) lorebook: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreparedContent {
    pub(crate) format: PreparedContentFormat,
    pub(crate) metadata: Value,
    pub(crate) assets: Vec<PreparedContentAsset>,
    pub(crate) cas_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) portrait_logical_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) module: Option<PreparedContentModule>,
}

#[derive(Debug)]
pub(crate) struct OpenedJobSource {
    pub(crate) file: File,
    pub(crate) total_bytes: u64,
}

fn open_regular_file_no_follow(path: &Path) -> Result<OpenedJobSource, NativeJobError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path).map_err(|error| {
        invalid_source_error(format!(
            "source cannot be opened without following links: {error}"
        ))
    })?;
    let metadata = file.metadata().map_err(|error| {
        invalid_source_error(format!("source metadata is unavailable: {error}"))
    })?;
    #[cfg(windows)]
    let is_reparse_point = {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    };
    #[cfg(not(windows))]
    let is_reparse_point = false;
    if metadata.file_type().is_symlink() || is_reparse_point || !metadata.is_file() {
        return Err(invalid_source_error("source must be a regular file"));
    }
    Ok(OpenedJobSource {
        file,
        total_bytes: metadata.len(),
    })
}

pub(crate) fn open_job_source(
    job_root: &Path,
    source: &JobSource,
) -> Result<OpenedJobSource, NativeJobError> {
    match source {
        JobSource::DesktopPath { path } => {
            let path = Path::new(path);
            if !path.is_absolute() || path.file_name().is_none() {
                return Err(invalid_source_error(
                    "desktop source must be an absolute file path",
                ));
            }
            open_regular_file_no_follow(path)
        }
        JobSource::AndroidSpool { token } => {
            let path = resolve_spool_source(job_root, token)?;
            open_regular_file_no_follow(&path)
        }
    }
}

fn validate_desktop_destination(path: &Path) -> Result<(), NativeJobError> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .parent()
            .and_then(|parent| parent.canonicalize().ok())
            .is_none()
    {
        return Err(NativeJobError::new(
            "invalid-destination",
            "desktop export destination must have an available absolute parent directory",
        ));
    }
    Ok(())
}

#[cfg(test)]
fn resolve_source(job_root: &Path, source: &JobSource) -> Result<PathBuf, NativeJobError> {
    match source {
        JobSource::DesktopPath { path } => resolve_desktop_source(path),
        JobSource::AndroidSpool { token } => resolve_spool_source(job_root, token),
    }
}

#[cfg(test)]
fn resolve_desktop_source(path: &str) -> Result<PathBuf, NativeJobError> {
    let path = Path::new(path).canonicalize().map_err(|error| {
        NativeJobError::new(
            "invalid-source",
            format!("desktop source is unavailable: {error}"),
        )
    })?;
    if !path
        .metadata()
        .map_err(|error| {
            NativeJobError::new(
                "invalid-source",
                format!("desktop source metadata is unavailable: {error}"),
            )
        })?
        .is_file()
    {
        return Err(NativeJobError::new(
            "invalid-source",
            "desktop source must be a regular file",
        ));
    }
    Ok(path)
}

fn parse_android_spool_token(token: &str) -> Result<Uuid, NativeJobError> {
    let parsed =
        Uuid::parse_str(token).map_err(|_| invalid_source_error("invalid Android spool token"))?;
    if parsed.hyphenated().to_string() != token
        || parsed.get_version() != Some(uuid::Version::Random)
        || parsed.get_variant() != uuid::Variant::RFC4122
    {
        return Err(invalid_source_error("invalid Android spool token"));
    }
    Ok(parsed)
}

fn resolve_spool_source(job_root: &Path, token: &str) -> Result<PathBuf, NativeJobError> {
    parse_android_spool_token(token)?;
    let sources_root = job_root.join("sources").canonicalize().map_err(|error| {
        invalid_source_error(format!("Android source root is unavailable: {error}"))
    })?;
    let spool = sources_root.join(token);
    let canonical_spool = spool.canonicalize().map_err(|error| {
        invalid_source_error(format!("Android spool directory is unavailable: {error}"))
    })?;
    if canonical_spool.parent() != Some(sources_root.as_path())
        || canonical_spool.file_name().and_then(|name| name.to_str()) != Some(token)
    {
        return Err(invalid_source_error(
            "Android spool owned directory escapes its canonical source root",
        ));
    }
    validate_spool_source(&canonical_spool, token, None)
}

fn claim_spool_source(
    job_root: &Path,
    token: &str,
    owned_directory: &Path,
) -> Result<PathBuf, NativeJobError> {
    claim_spool_source_with_display_name(job_root, token, owned_directory, None)
}

fn claim_spool_content_source(
    job_root: &Path,
    token: &str,
    owned_directory: &Path,
    display_name: &str,
) -> Result<PathBuf, NativeJobError> {
    claim_spool_source_with_display_name(job_root, token, owned_directory, Some(display_name))
}

fn claim_spool_source_with_display_name(
    job_root: &Path,
    token: &str,
    owned_directory: &Path,
    expected_display_name: Option<&str>,
) -> Result<PathBuf, NativeJobError> {
    parse_android_spool_token(token)?;
    let sources_root = job_root.join("sources").canonicalize().map_err(|error| {
        invalid_source_error(format!("Android source root is unavailable: {error}"))
    })?;
    let pending = sources_root.join(token);
    let pending_type = fs::symlink_metadata(&pending).map_err(|error| {
        invalid_source_error(format!("Android spool directory is unavailable: {error}"))
    })?;
    if !pending_type.is_dir() || pending_type.file_type().is_symlink() {
        return Err(invalid_source_error(
            "Android spool owned directory is invalid",
        ));
    }
    let canonical_owned = owned_directory.canonicalize().map_err(|error| {
        invalid_source_error(format!("native job directory is unavailable: {error}"))
    })?;
    let claimed = canonical_owned.join("android-source");
    fs::rename(&pending, &claimed).map_err(|error| {
        invalid_source_error(format!("Android spool token is already claimed: {error}"))
    })?;
    let canonical_claimed = claimed.canonicalize().map_err(|error| {
        invalid_source_error(format!("claimed Android spool is unavailable: {error}"))
    })?;
    if canonical_claimed.parent() != Some(canonical_owned.as_path())
        || canonical_claimed.file_name().and_then(|name| name.to_str()) != Some("android-source")
    {
        return Err(invalid_source_error(
            "claimed Android spool escapes its native job directory",
        ));
    }
    validate_spool_source(&canonical_claimed, token, expected_display_name)
}

fn validate_spool_source(
    canonical_spool: &Path,
    token: &str,
    expected_display_name: Option<&str>,
) -> Result<PathBuf, NativeJobError> {
    let ownership_path = canonical_spool.join("ownership.json");
    let ownership_metadata = ownership_path.metadata().map_err(|error| {
        invalid_source_error(format!("Android spool ownership is unavailable: {error}"))
    })?;
    if !ownership_metadata.is_file() || ownership_metadata.len() > MAX_MANIFEST_BYTES {
        return Err(invalid_source_error("Android spool ownership is invalid"));
    }
    let ownership: SpoolOwnership =
        serde_json::from_slice(&fs::read(&ownership_path).map_err(|error| {
            invalid_source_error(format!("Android spool ownership cannot be read: {error}"))
        })?)
        .map_err(|error| {
            invalid_source_error(format!("Android spool ownership is invalid: {error}"))
        })?;
    if ownership.format != ANDROID_SPOOL_FORMAT
        || ownership.version != ANDROID_SPOOL_VERSION
        || ownership.token != token
    {
        return Err(invalid_source_error(
            "Android spool ownership does not match its token",
        ));
    }
    let manifest_path = canonical_spool.join("source.json");
    let manifest_metadata = manifest_path.metadata().map_err(|error| {
        invalid_source_error(format!("Android spool manifest is unavailable: {error}"))
    })?;
    if !manifest_metadata.is_file() || manifest_metadata.len() > MAX_MANIFEST_BYTES {
        return Err(invalid_source_error("Android spool manifest is invalid"));
    }
    let manifest: SpoolManifest =
        serde_json::from_slice(&fs::read(&manifest_path).map_err(|error| {
            invalid_source_error(format!("Android spool manifest cannot be read: {error}"))
        })?)
        .map_err(|error| {
            invalid_source_error(format!("Android spool manifest is invalid: {error}"))
        })?;
    if manifest.token != token
        || manifest.state != SpoolState::Ready
        || !is_safe_spool_display_name(&manifest.display_name)
        || expected_display_name.is_some_and(|name| name != manifest.display_name)
    {
        return Err(invalid_source_error("Android spool source is not ready"));
    }
    let source = canonical_spool.join("source.risudat");
    let metadata = fs::symlink_metadata(&source).map_err(|error| {
        invalid_source_error(format!(
            "Android spool source metadata is unavailable: {error}"
        ))
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || manifest.bytes != Some(metadata.len())
    {
        return Err(invalid_source_error(
            "Android spool source does not match its ready manifest",
        ));
    }
    Ok(source)
}

fn is_safe_spool_display_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 180
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn invalid_source_error(message: impl AsRef<str>) -> NativeJobError {
    NativeJobError::new("invalid-source", message)
}

fn cleanup_owned_directories(jobs_root: &Path) -> Result<(), String> {
    if !jobs_root.is_dir() {
        return Ok(());
    }
    let canonical_root = jobs_root
        .canonicalize()
        .map_err(|error| format!("native job root cannot be resolved: {error}"))?;
    let mut errors = Vec::new();
    for entry in fs::read_dir(&canonical_root)
        .map_err(|error| format!("native job root cannot be read: {error}"))?
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                record_cleanup_error(
                    &mut errors,
                    format!("native job entry cannot be read: {error}"),
                );
                continue;
            }
        };
        let is_directory = match entry.file_type() {
            Ok(file_type) => file_type.is_dir(),
            Err(error) => {
                record_cleanup_error(
                    &mut errors,
                    format!("native job entry type is unavailable: {error}"),
                );
                continue;
            }
        };
        if !is_directory {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(&name) else {
            continue;
        };
        if id.hyphenated().to_string() != name {
            continue;
        }
        let manifest_path = entry.path().join("ownership.json");
        let Ok(metadata) = manifest_path.metadata() else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
            continue;
        }
        let Ok(bytes) = fs::read(&manifest_path) else {
            continue;
        };
        let Ok(ownership) = serde_json::from_slice::<JobOwnership>(&bytes) else {
            continue;
        };
        if ownership.job_id != name {
            continue;
        }
        let owned = match entry.path().canonicalize() {
            Ok(owned) => owned,
            Err(error) => {
                record_cleanup_error(
                    &mut errors,
                    format!("native job directory cannot be resolved: {error}"),
                );
                continue;
            }
        };
        if owned.parent() != Some(canonical_root.as_path()) {
            continue;
        }
        if let Err(error) = fs::remove_dir_all(&owned) {
            record_cleanup_error(
                &mut errors,
                format!("native job directory cannot be removed: {error}"),
            );
        }
    }
    cleanup_errors_result(errors)
}

fn cleanup_spool_directories(sources_root: &Path) -> Result<(), String> {
    let now_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64;
    cleanup_spool_directories_at(sources_root, now_millis, ANDROID_SPOOL_STALE_MILLIS)
}

fn lossless_handoff_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(token) = name
        .strip_prefix("risulossless-")
        .and_then(|name| name.strip_suffix(".risulossless"))
    else {
        return false;
    };
    Uuid::parse_str(token).is_ok_and(|id| {
        id.hyphenated().to_string() == token
            && id.get_version() == Some(uuid::Version::Random)
            && id.get_variant() == uuid::Variant::RFC4122
    })
}

fn cleanup_lossless_handoff_path(root: &Path, path: &Path) -> Result<bool, NativeJobError> {
    let handoffs_root = root.join("handoffs").canonicalize().map_err(|error| {
        NativeJobError::new(
            "cleanup-failed",
            format!("lossless handoff root cannot be resolved: {error}"),
        )
    })?;
    if !path.is_absolute()
        || path.parent().and_then(|parent| parent.canonicalize().ok())
            != Some(handoffs_root.clone())
        || !lossless_handoff_name(path)
    {
        return Err(NativeJobError::new(
            "invalid-input",
            "lossless handoff cleanup target is not app-owned",
        ));
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(NativeJobError::new(
                "cleanup-failed",
                format!("lossless handoff metadata is unavailable: {error}"),
            ))
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(NativeJobError::new(
            "invalid-input",
            "lossless handoff cleanup target is not an owned regular file",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(NativeJobError::new(
                "invalid-input",
                "lossless handoff cleanup target is a reparse point",
            ));
        }
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(NativeJobError::new(
            "cleanup-failed",
            format!("lossless handoff cannot be removed: {error}"),
        )),
    }
}

fn cleanup_spool_directories_at(
    sources_root: &Path,
    now_millis: u64,
    stale_after_millis: u64,
) -> Result<(), String> {
    if !sources_root.is_dir() {
        return Ok(());
    }
    let canonical_root = sources_root
        .canonicalize()
        .map_err(|error| format!("native source root cannot be resolved: {error}"))?;
    let mut errors = Vec::new();
    for entry in fs::read_dir(&canonical_root)
        .map_err(|error| format!("native source root cannot be read: {error}"))?
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                record_cleanup_error(
                    &mut errors,
                    format!("native source entry cannot be read: {error}"),
                );
                continue;
            }
        };
        let is_directory = match entry.file_type() {
            Ok(file_type) => file_type.is_dir(),
            Err(error) if is_spool_cleanup_race_loss(&error, &entry.path()) => continue,
            Err(error) => {
                record_cleanup_error(
                    &mut errors,
                    format!("native source entry type is unavailable: {error}"),
                );
                continue;
            }
        };
        if !is_directory {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let (token, is_staging, is_cleanup) =
            if let Some(token) = name.strip_prefix(ANDROID_SPOOL_STAGING_PREFIX) {
                (token, true, false)
            } else if let Some(token) = name.strip_prefix(ANDROID_SPOOL_CLEANUP_PREFIX) {
                (token, false, true)
            } else {
                (name.as_str(), false, false)
            };
        if parse_android_spool_token(token).is_err() {
            continue;
        }
        let owned = match entry.path().canonicalize() {
            Ok(owned) => owned,
            Err(error) if is_spool_cleanup_race_loss(&error, &entry.path()) => continue,
            Err(error) => {
                record_cleanup_error(
                    &mut errors,
                    format!("native source directory cannot be resolved: {error}"),
                );
                continue;
            }
        };
        if owned.parent() != Some(canonical_root.as_path()) {
            continue;
        }
        let ownership_path = owned.join("ownership.json");
        let ownership = ownership_path
            .metadata()
            .ok()
            .filter(|metadata| metadata.is_file() && metadata.len() <= MAX_MANIFEST_BYTES)
            .and_then(|_| fs::read(&ownership_path).ok())
            .and_then(|bytes| serde_json::from_slice::<SpoolOwnership>(&bytes).ok())
            .filter(|ownership| {
                ownership.format == ANDROID_SPOOL_FORMAT
                    && ownership.version == ANDROID_SPOOL_VERSION
                    && ownership.token == token
            });
        if is_cleanup {
            if ownership.is_some() {
                match fs::remove_dir_all(&owned) {
                    Ok(()) => {}
                    Err(error) if is_spool_cleanup_race_loss(&error, &owned) => {}
                    Err(error) => record_cleanup_error(
                        &mut errors,
                        format!("native source cleanup directory cannot be removed: {error}"),
                    ),
                }
            }
            continue;
        }
        let created_at_millis = ownership
            .as_ref()
            .map(|ownership| ownership.created_at_millis)
            .or_else(|| {
                is_staging.then(|| {
                    entry
                        .metadata()
                        .ok()
                        .and_then(|metadata| metadata.modified().ok())
                        .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
                        .unwrap_or(now_millis)
                })
            });
        let Some(created_at_millis) = created_at_millis else {
            continue;
        };
        if now_millis.saturating_sub(created_at_millis) < stale_after_millis {
            continue;
        }
        let cleanup = canonical_root.join(format!("{ANDROID_SPOOL_CLEANUP_PREFIX}{token}"));
        if cleanup.is_dir() {
            let cleanup_ownership_path = cleanup.join("ownership.json");
            let cleanup_is_owned = cleanup_ownership_path
                .metadata()
                .ok()
                .filter(|metadata| metadata.is_file() && metadata.len() <= MAX_MANIFEST_BYTES)
                .and_then(|_| fs::read(&cleanup_ownership_path).ok())
                .and_then(|bytes| serde_json::from_slice::<SpoolOwnership>(&bytes).ok())
                .is_some_and(|ownership| {
                    ownership.format == ANDROID_SPOOL_FORMAT
                        && ownership.version == ANDROID_SPOOL_VERSION
                        && ownership.token == token
                });
            if !cleanup_is_owned {
                continue;
            }
            match fs::remove_dir_all(&cleanup) {
                Ok(()) => {}
                Err(error) if is_spool_cleanup_race_loss(&error, &cleanup) => {}
                Err(error) => {
                    record_cleanup_error(
                        &mut errors,
                        format!("native source cleanup directory cannot be removed: {error}"),
                    );
                    continue;
                }
            }
        }
        match fs::rename(&owned, &cleanup) {
            Ok(()) => {}
            Err(error) if is_spool_cleanup_race_loss(&error, &owned) => continue,
            Err(error) => {
                record_cleanup_error(
                    &mut errors,
                    format!("native source cleanup claim failed: {error}"),
                );
                continue;
            }
        }
        match fs::remove_dir_all(&cleanup) {
            Ok(()) => {}
            Err(error) if is_spool_cleanup_race_loss(&error, &cleanup) => {}
            Err(error) => record_cleanup_error(
                &mut errors,
                format!("native source directory cannot be removed: {error}"),
            ),
        }
    }
    cleanup_errors_result(errors)
}

fn is_spool_cleanup_race_loss(error: &std::io::Error, path: &Path) -> bool {
    if error.kind() == std::io::ErrorKind::NotFound {
        return true;
    }
    if !cfg!(windows) || error.kind() != std::io::ErrorKind::PermissionDenied {
        return false;
    }
    for _ in 0..16 {
        match fs::symlink_metadata(path) {
            Err(probe) if probe.kind() == std::io::ErrorKind::NotFound => return true,
            _ => std::thread::yield_now(),
        }
    }
    matches!(
        fs::symlink_metadata(path),
        Err(probe) if probe.kind() == std::io::ErrorKind::NotFound
    )
}

fn record_cleanup_error(errors: &mut Vec<String>, error: String) {
    if errors.len() < MAX_CLEANUP_ERRORS {
        errors.push(error);
    }
}

fn cleanup_errors_result(errors: Vec<String>) -> Result<(), String> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

pub(crate) struct NativeFileJobState {
    root: PathBuf,
    registry: Arc<JobRegistry>,
    active_workers: Arc<AtomicUsize>,
    max_concurrent_jobs: usize,
    startup_warnings: Vec<NativeJobError>,
    capability_error: Option<NativeJobError>,
}

impl NativeFileJobState {
    pub(crate) fn initialize(root: PathBuf) -> Self {
        Self::initialize_with_max_workers(root, MAX_CONCURRENT_JOBS)
    }

    fn initialize_with_max_workers(root: PathBuf, max_concurrent_jobs: usize) -> Self {
        let jobs_root = root.join("jobs");
        let sources_root = root.join("sources");
        let handoffs_root = root.join("handoffs");
        let mut startup_warnings = Vec::new();
        let mut capability_error = None;
        for (path, label) in [
            (&jobs_root, "native job"),
            (&sources_root, "native source"),
            (&handoffs_root, "native lossless handoff"),
        ] {
            if let Err(error) = fs::create_dir_all(path) {
                let error = NativeJobError::new(
                    "capability-unavailable",
                    format!("{label} root cannot be created: {error}"),
                );
                startup_warnings.push(error.clone());
                capability_error.get_or_insert(error);
            }
        }
        if jobs_root.is_dir() {
            if let Err(error) = cleanup_owned_directories(&jobs_root) {
                startup_warnings.push(NativeJobError::new("cleanup-failed", error));
            }
        }
        if sources_root.is_dir() {
            if let Err(error) = cleanup_spool_directories(&sources_root) {
                startup_warnings.push(NativeJobError::new("cleanup-failed", error));
            }
        }
        startup_warnings.truncate(MAX_WARNING_CODES);
        Self {
            root,
            registry: Arc::new(JobRegistry::default()),
            active_workers: Arc::new(AtomicUsize::new(0)),
            max_concurrent_jobs,
            startup_warnings,
            capability_error,
        }
    }

    pub(crate) fn start(
        &self,
        request: NativeFileJobStartRequest,
        app: AppHandle,
    ) -> Result<NativeFileJobStarted, NativeJobError> {
        if let Some(error) = &self.capability_error {
            return Err(error.clone());
        }
        let task = match request {
            NativeFileJobStartRequest::RestoreBlockRisuSave {
                source,
                expected_revision,
            } => {
                let opened_source = match &source {
                    JobSource::DesktopPath { .. } => Some(open_job_source(&self.root, &source)?),
                    JobSource::AndroidSpool { token } => {
                        parse_android_spool_token(token)?;
                        None
                    }
                };
                NativeFileJobTask::Restore {
                    opened_source,
                    source,
                    expected_revision,
                    sink: RestoreJobSink::Persistent(app),
                }
            }
            NativeFileJobStartRequest::ExportBlockRisuSave {
                destination,
                expected_revision,
                omit_account,
            } => {
                let destination = PathBuf::from(destination);
                if !destination.is_absolute()
                    || destination.file_name().is_none()
                    || destination
                        .parent()
                        .and_then(|parent| parent.canonicalize().ok())
                        .is_none()
                {
                    return Err(NativeJobError::new(
                        "invalid-destination",
                        "desktop export destination must have an available absolute parent directory",
                    ));
                }
                NativeFileJobTask::Export {
                    destination,
                    expected_revision,
                    omit_account,
                    app,
                }
            }
            NativeFileJobStartRequest::RestoreLosslessBackup {
                source,
                expected_revision,
            } => {
                let opened_source = match &source {
                    JobSource::DesktopPath { .. } => Some(open_job_source(&self.root, &source)?),
                    JobSource::AndroidSpool { token } => {
                        parse_android_spool_token(token)?;
                        None
                    }
                };
                let store =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        store.open_native_job_store()
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::RestoreLossless {
                    opened_source,
                    source,
                    expected_revision,
                    store,
                }
            }
            NativeFileJobStartRequest::RestoreOfficialAccountSnapshot {
                base_url,
                credential,
                expected_revision,
            } => {
                let store =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        store.open_native_job_store()
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::RestoreOfficialSnapshot {
                    request: official_snapshot::OfficialSnapshotRestoreRequest {
                        base_url,
                        credential,
                    },
                    expected_revision,
                    store,
                    app,
                }
            }
            NativeFileJobStartRequest::ExportLosslessBackup {
                destination,
                expected_revision,
            } => {
                let destination = destination.map(PathBuf::from);
                if let Some(destination) = destination.as_deref() {
                    validate_desktop_destination(destination)?;
                }
                let store =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        store.open_native_job_store()
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::ExportLossless {
                    destination,
                    expected_revision,
                    store,
                }
            }
            request @ NativeFileJobStartRequest::PrepareContentImport { .. } => {
                return self.start_content(request);
            }
            NativeFileJobStartRequest::KeiBackupUpload {
                lease,
                expected_revision,
                url,
                expected_account_id,
                token,
            } => {
                #[cfg(feature = "native-kei-upload-pilot")]
                {
                    let prepared =
                        crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                            store.prepare_kei_job_upload(
                                &lease,
                                expected_revision,
                                &url,
                                &expected_account_id,
                                &token,
                            )
                        })
                        .map_err(native_store_error)?;
                    NativeFileJobTask::KeiBackup { prepared }
                }
                #[cfg(not(feature = "native-kei-upload-pilot"))]
                {
                    let _ = (lease, expected_revision, url, expected_account_id, token);
                    return Err(NativeJobError::new(
                        "capability-unavailable",
                        "native KEI backup jobs are not compiled in this build",
                    ));
                }
            }
        };
        self.spawn(task, true)
    }

    #[allow(dead_code)]
    fn start_content(
        &self,
        request: NativeFileJobStartRequest,
    ) -> Result<NativeFileJobStarted, NativeJobError> {
        if let Some(error) = &self.capability_error {
            return Err(error.clone());
        }
        let NativeFileJobStartRequest::PrepareContentImport {
            source,
            display_name,
        } = request
        else {
            return Err(NativeJobError::new(
                "invalid-input",
                "content preparation requires a content import request",
            ));
        };
        if !is_bounded_content_display_name(&display_name) {
            return Err(NativeJobError::new(
                "invalid-input",
                "content import display name is invalid",
            ));
        }
        let repository_root = self
            .root
            .parent()
            .ok_or_else(|| {
                NativeJobError::new(
                    "capability-unavailable",
                    "native content repository root is unavailable",
                )
            })?
            .to_path_buf();
        let worker_permit =
            WorkerPermit::acquire(Arc::clone(&self.active_workers), self.max_concurrent_jobs)?;
        let warning_codes = self
            .startup_warnings
            .iter()
            .map(|warning| warning.code.clone())
            .collect::<Vec<_>>();
        let job = self
            .registry
            .create_internal(
                JobKind::PrepareContentImport,
                None,
                warning_codes.clone(),
                false,
            )
            .map_err(|error| NativeJobError::new("store-error", error))?;
        let job_id = job.id();
        let owned_directory = match create_owned_directory(&self.root.join("jobs"), &job_id) {
            Ok(path) => path,
            Err(error) => {
                let _ = job.finish_failure("capability-unavailable", &error);
                let _ = self.registry.forget(&job_id);
                return Err(NativeJobError::new("capability-unavailable", error));
            }
        };
        let opened_source = match &source {
            JobSource::DesktopPath { .. } => open_job_source(&self.root, &source),
            JobSource::AndroidSpool { token } => {
                claim_spool_content_source(&self.root, token, &owned_directory, &display_name)
                    .and_then(|path| open_regular_file_no_follow(&path))
            }
        };
        let opened_source = match opened_source {
            Ok(source) => source,
            Err(error) => {
                let cleanup =
                    cleanup_one_owned_directory(&self.root.join("jobs"), &owned_directory, &job_id);
                let _ = job.finish_failure(&error.code, &error.message);
                let _ = self.registry.forget(&job_id);
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(NativeJobError::new(
                        "cleanup-failed",
                        format!("{}; cleanup failed: {cleanup}", error.message),
                    )),
                };
            }
        };
        let root = self.root.clone();
        let registry = Arc::clone(&self.registry);
        std::thread::spawn(move || {
            let outcome = content::prepare_content(
                opened_source,
                &display_name,
                &owned_directory,
                &repository_root,
                &job,
            );
            let cleanup =
                cleanup_one_owned_directory(&root.join("jobs"), &owned_directory, &job.id());
            drop(worker_permit);
            let _ = job.finish_content_job_with_cas(outcome, cleanup, &repository_root);
            let _ = registry.prune();
        });
        Ok(NativeFileJobStarted {
            job_id,
            warning_codes,
        })
    }

    #[cfg(test)]
    pub(crate) fn start_content_for_test(
        &self,
        request: NativeFileJobStartRequest,
    ) -> Result<NativeFileJobStarted, NativeJobError> {
        self.start_content(request)
    }

    #[cfg(test)]
    pub(crate) fn start_with_sink(
        &self,
        request: NativeFileJobStartRequest,
        sink: Arc<dyn restore::ReplacementSink>,
    ) -> Result<NativeFileJobStarted, NativeJobError> {
        let NativeFileJobStartRequest::RestoreBlockRisuSave {
            source,
            expected_revision,
        } = request
        else {
            return Err(NativeJobError::new(
                "invalid-input",
                "test replacement sink only supports restore jobs",
            ));
        };
        let opened_source = match &source {
            JobSource::DesktopPath { .. } => Some(open_job_source(&self.root, &source)?),
            JobSource::AndroidSpool { token } => {
                parse_android_spool_token(token)?;
                None
            }
        };
        self.spawn(
            NativeFileJobTask::Restore {
                opened_source,
                source,
                expected_revision,
                sink: RestoreJobSink::Test(sink),
            },
            false,
        )
    }

    fn spawn(
        &self,
        mut task: NativeFileJobTask,
        require_restore_finalization: bool,
    ) -> Result<NativeFileJobStarted, NativeJobError> {
        let worker_permit =
            WorkerPermit::acquire(Arc::clone(&self.active_workers), self.max_concurrent_jobs)?;
        let kind = task.kind();
        let warning_codes = self
            .startup_warnings
            .iter()
            .map(|warning| warning.code.clone())
            .collect::<Vec<_>>();
        let job = self
            .registry
            .create_internal(
                kind,
                Some(task.expected_revision()),
                warning_codes.clone(),
                require_restore_finalization
                    && matches!(
                        kind,
                        JobKind::RestoreBlockRisuSave
                            | JobKind::RestoreLosslessBackup
                            | JobKind::RestoreOfficialAccountSnapshot
                    ),
            )
            .map_err(|error| NativeJobError::new("store-error", error))?;
        let job_id = job.id();
        let owned_directory = match create_owned_directory(&self.root.join("jobs"), &job_id) {
            Ok(path) => path,
            Err(error) => {
                let _ = job.finish_failure("capability-unavailable", &error);
                let _ = self.registry.forget(&job_id);
                return Err(NativeJobError::new("capability-unavailable", error));
            }
        };
        let source_preparation = (|| -> Result<(), NativeJobError> {
            let (opened_source, source) = match &mut task {
                NativeFileJobTask::Restore {
                    opened_source,
                    source,
                    ..
                }
                | NativeFileJobTask::RestoreLossless {
                    opened_source,
                    source,
                    ..
                } => (opened_source, source),
                _ => return Ok(()),
            };
            match source {
                JobSource::DesktopPath { .. } if opened_source.is_some() => Ok(()),
                JobSource::AndroidSpool { token } if opened_source.is_none() => {
                    let path = claim_spool_source(&self.root, token, &owned_directory)?;
                    *opened_source = Some(open_regular_file_no_follow(&path)?);
                    Ok(())
                }
                _ => Err(NativeJobError::new(
                    "store-error",
                    "native job source resolution is inconsistent",
                )),
            }
        })();
        if let Err(error) = source_preparation {
            let cleanup =
                cleanup_one_owned_directory(&self.root.join("jobs"), &owned_directory, &job_id);
            let _ = job.finish_failure(&error.code, &error.message);
            let _ = self.registry.forget(&job_id);
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(NativeJobError::new(
                    "cleanup-failed",
                    format!("{}; cleanup failed: {cleanup}", error.message),
                )),
            };
        };
        let root = self.root.clone();
        let registry = Arc::clone(&self.registry);
        std::thread::spawn(move || {
            let _worker_permit = worker_permit;
            let outcome = match task {
                NativeFileJobTask::Restore {
                    opened_source,
                    expected_revision,
                    sink,
                    ..
                } => match opened_source {
                    Some(opened_source) => match sink {
                        RestoreJobSink::Persistent(app) => restore::restore_block_risu_save(
                            opened_source,
                            expected_revision,
                            &job,
                            &PersistentReplacementSink { app },
                        ),
                        #[cfg(test)]
                        RestoreJobSink::Test(sink) => restore::restore_block_risu_save(
                            opened_source,
                            expected_revision,
                            &job,
                            sink.as_ref(),
                        ),
                    },
                    None => Err(NativeJobError::new(
                        "store-error",
                        "native job source was not prepared",
                    )),
                },
                NativeFileJobTask::Export {
                    destination,
                    expected_revision,
                    omit_account,
                    app,
                } => crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                    store.prepare_risu_save_export(expected_revision)
                })
                .map_err(native_store_error)
                .and_then(|prepared| {
                    export::export_block_risu_save(prepared, &destination, omit_account, &job)
                }),
                NativeFileJobTask::RestoreLossless {
                    opened_source,
                    expected_revision,
                    store,
                    ..
                } => match opened_source {
                    Some(opened_source) => lossless::restore_lossless_backup(
                        opened_source,
                        expected_revision,
                        &owned_directory,
                        store,
                        &job,
                    ),
                    None => Err(NativeJobError::new(
                        "store-error",
                        "native lossless job source was not prepared",
                    )),
                },
                NativeFileJobTask::RestoreOfficialSnapshot {
                    request,
                    expected_revision,
                    store,
                    app,
                } => official_snapshot::restore_official_snapshot(
                    request,
                    expected_revision,
                    &owned_directory,
                    store,
                    &job,
                    &PersistentReplacementSink { app },
                ),
                NativeFileJobTask::ExportLossless {
                    destination,
                    expected_revision,
                    store,
                } => lossless::export_lossless_backup(
                    destination.as_deref(),
                    expected_revision,
                    &owned_directory,
                    &root.join("handoffs"),
                    store,
                    &job,
                ),
                #[cfg(feature = "native-kei-upload-pilot")]
                NativeFileJobTask::KeiBackup { prepared } => {
                    crate::persistent_store::kei::run_job(prepared, Arc::clone(&job))
                }
            };
            let mut cleanup_errors = Vec::new();
            if let Err(error) =
                cleanup_one_owned_directory(&root.join("jobs"), &owned_directory, &job.id())
            {
                cleanup_errors.push(error);
            }
            match (outcome, cleanup_errors.is_empty()) {
                (Ok(result), true) => {
                    let _ = job.finish_success(result);
                }
                (Ok(mut result), false) => {
                    if !result
                        .warning_codes
                        .iter()
                        .any(|code| code == "cleanup-failed")
                    {
                        result.warning_codes.push("cleanup-failed".to_owned());
                    }
                    let _ = job.finish_success(result);
                }
                (Err(error), false) => {
                    let cleanup = cleanup_errors.join("; ");
                    let _ = job.finish_failure(
                        "cleanup-failed",
                        &format!("{}; cleanup failed: {cleanup}", error.message),
                    );
                }
                (Err(error), true) if error.code == "cancelled" => {
                    let _ = job.finish_cancelled();
                }
                (Err(error), true) => {
                    let _ = job.finish_failure(&error.code, &error.message);
                }
            }
            let _ = registry.prune();
        });
        Ok(NativeFileJobStarted {
            job_id,
            warning_codes,
        })
    }

    pub(crate) fn status(&self, job_id: &str) -> Result<JobStatus, NativeJobError> {
        self.registry
            .status(job_id)
            .map_err(|error| NativeJobError::new("store-error", error))
    }

    pub(crate) fn content_asset_receipt(
        &self,
        job_id: &str,
    ) -> Result<Vec<(String, u64)>, NativeJobError> {
        let job = self
            .registry
            .lookup(job_id)
            .map_err(|error| NativeJobError::new("store-error", error))?
            .ok_or_else(|| NativeJobError::new("invalid-input", "native content job is missing"))?;
        let status = job
            .status
            .lock()
            .map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
        if status.kind != JobKind::PrepareContentImport
            || status.state != JobState::Succeeded
            || status.phase != JobPhase::Complete
        {
            return Err(NativeJobError::new(
                "invalid-input",
                "native content receipt is not terminal and successful",
            ));
        }
        let prepared = status.prepared_content.as_ref().ok_or_else(|| {
            NativeJobError::new(
                "invalid-input",
                "successful native content job has no prepared receipt",
            )
        })?;
        if prepared.cas_session_id != job_id {
            return Err(NativeJobError::new(
                "invalid-input",
                "native content receipt CAS session does not match its job",
            ));
        }
        Ok(prepared
            .assets
            .iter()
            .map(|asset| (asset.object_hash.clone(), asset.byte_size))
            .collect())
    }

    pub(crate) fn list(&self) -> Result<Vec<JobStatus>, NativeJobError> {
        self.registry
            .list()
            .map_err(|error| NativeJobError::new("store-error", error))
    }

    pub(crate) fn finalize(&self, job_id: &str) -> Result<FinalizeOutcome, NativeJobError> {
        self.registry
            .finalize(job_id)
            .map_err(|error| NativeJobError::new("store-error", error))
    }

    pub(crate) fn cancel(&self, job_id: &str) -> Result<CancelOutcome, NativeJobError> {
        self.registry
            .cancel(job_id)
            .map_err(|error| NativeJobError::new("store-error", error))
    }

    pub(crate) fn forget(&self, job_id: &str) -> Result<bool, NativeJobError> {
        self.registry
            .forget(job_id)
            .map_err(|error| NativeJobError::new("store-error", error))
    }
}

fn is_bounded_content_display_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 180
        && !name.chars().any(|character| character.is_control())
}

fn content_cancelled() -> NativeJobError {
    NativeJobError::new("cancelled", "content preparation was cancelled")
}

fn finish_content_cas_session(
    repository_root: &Path,
    expected_job_id: &str,
    cancel_requested: bool,
    outcome: &Result<PreparedContent, NativeJobError>,
    cleanup: Result<(), String>,
) -> Result<(), String> {
    let Ok(prepared) = outcome else {
        return cleanup;
    };
    if cleanup.is_ok() && !cancel_requested {
        return cleanup;
    }
    if prepared.cas_session_id != expected_job_id {
        return combine_cleanup_errors(
            cleanup,
            "prepared content CAS session does not match its native job".to_owned(),
        );
    }
    let abort = crate::asset_repository::job_pins::DurableCasJob::open(
        repository_root,
        &prepared.cas_session_id,
    )
    .and_then(|mut session| {
        session.release(crate::asset_repository::job_pins::CasReleaseOutcome::Aborted)
    })
    .map_err(|error| format!("content CAS session abort failed: {error}"));
    match abort {
        Ok(()) => cleanup,
        Err(abort) => combine_cleanup_errors(cleanup, abort),
    }
}

fn combine_cleanup_errors(cleanup: Result<(), String>, additional: String) -> Result<(), String> {
    match cleanup {
        Ok(()) => Err(additional),
        Err(cleanup) => Err(format!("{cleanup}; {additional}")),
    }
}

enum RestoreJobSink {
    Persistent(AppHandle),
    #[cfg(test)]
    Test(Arc<dyn restore::ReplacementSink>),
}

enum NativeFileJobTask {
    Restore {
        opened_source: Option<OpenedJobSource>,
        source: JobSource,
        expected_revision: i64,
        sink: RestoreJobSink,
    },
    Export {
        destination: PathBuf,
        expected_revision: i64,
        omit_account: bool,
        app: AppHandle,
    },
    RestoreLossless {
        opened_source: Option<OpenedJobSource>,
        source: JobSource,
        expected_revision: i64,
        store: crate::persistent_store::PersistentStore,
    },
    RestoreOfficialSnapshot {
        request: official_snapshot::OfficialSnapshotRestoreRequest,
        expected_revision: i64,
        store: crate::persistent_store::PersistentStore,
        app: AppHandle,
    },
    ExportLossless {
        destination: Option<PathBuf>,
        expected_revision: i64,
        store: crate::persistent_store::PersistentStore,
    },
    #[cfg(feature = "native-kei-upload-pilot")]
    KeiBackup {
        prepared: crate::persistent_store::kei::PreparedKeiUpload,
    },
}

impl NativeFileJobTask {
    fn kind(&self) -> JobKind {
        match self {
            Self::Restore { .. } => JobKind::RestoreBlockRisuSave,
            Self::Export { .. } => JobKind::ExportBlockRisuSave,
            Self::RestoreLossless { .. } => JobKind::RestoreLosslessBackup,
            Self::RestoreOfficialSnapshot { .. } => JobKind::RestoreOfficialAccountSnapshot,
            Self::ExportLossless { .. } => JobKind::ExportLosslessBackup,
            #[cfg(feature = "native-kei-upload-pilot")]
            Self::KeiBackup { .. } => JobKind::KeiBackupUpload,
        }
    }

    fn expected_revision(&self) -> i64 {
        match self {
            Self::Restore {
                expected_revision, ..
            }
            | Self::Export {
                expected_revision, ..
            }
            | Self::RestoreLossless {
                expected_revision, ..
            }
            | Self::RestoreOfficialSnapshot {
                expected_revision, ..
            }
            | Self::ExportLossless {
                expected_revision, ..
            } => *expected_revision,
            #[cfg(feature = "native-kei-upload-pilot")]
            Self::KeiBackup { prepared } => prepared.revision(),
        }
    }
}

fn native_store_error(error: crate::persistent_store::StoreError) -> NativeJobError {
    let code = match error {
        crate::persistent_store::StoreError::RevisionConflict { .. } => "revision-conflict",
        crate::persistent_store::StoreError::Validation { .. } => "invalid-input",
        crate::persistent_store::StoreError::SnapshotReleased
        | crate::persistent_store::StoreError::Store { .. } => "store-error",
    };
    NativeJobError::new(code, error.to_string())
}

#[derive(Debug)]
struct WorkerPermit {
    active_workers: Arc<AtomicUsize>,
}

impl WorkerPermit {
    fn acquire(
        active_workers: Arc<AtomicUsize>,
        max_concurrent_jobs: usize,
    ) -> Result<Self, NativeJobError> {
        let reserved = active_workers.fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
            (active < max_concurrent_jobs).then_some(active + 1)
        });
        if reserved.is_err() {
            return Err(NativeJobError::new(
                "job-capacity",
                "native file job concurrency limit reached",
            ));
        }
        Ok(Self { active_workers })
    }
}

impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.active_workers.fetch_sub(1, Ordering::AcqRel);
    }
}

fn create_owned_directory(jobs_root: &Path, job_id: &str) -> Result<PathBuf, String> {
    let path = jobs_root.join(job_id);
    fs::create_dir(&path)
        .map_err(|error| format!("native job directory cannot be created: {error}"))?;
    let result = (|| {
        let manifest_path = path.join("ownership.json");
        let mut manifest = File::create(&manifest_path)
            .map_err(|error| format!("native job ownership cannot be created: {error}"))?;
        serde_json::to_writer(
            &mut manifest,
            &JobOwnership {
                job_id: job_id.to_owned(),
            },
        )
        .map_err(|error| format!("native job ownership cannot be written: {error}"))?;
        manifest
            .flush()
            .map_err(|error| format!("native job ownership cannot be flushed: {error}"))?;
        manifest
            .sync_all()
            .map_err(|error| format!("native job ownership cannot be synced: {error}"))?;
        Ok(path.clone())
    })();
    match result {
        Ok(path) => Ok(path),
        Err(error) => match fs::remove_dir_all(&path) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(format!(
                "{error}; partial native job directory cleanup failed: {cleanup}"
            )),
        },
    }
}

fn cleanup_one_owned_directory(
    jobs_root: &Path,
    owned_directory: &Path,
    job_id: &str,
) -> Result<(), String> {
    let canonical_root = jobs_root
        .canonicalize()
        .map_err(|error| format!("native job root cannot be resolved: {error}"))?;
    let canonical_owned = owned_directory
        .canonicalize()
        .map_err(|error| format!("native job directory cannot be resolved: {error}"))?;
    if canonical_owned.parent() != Some(canonical_root.as_path())
        || canonical_owned.file_name().and_then(|name| name.to_str()) != Some(job_id)
    {
        return Err("native job cleanup target is outside its owned root".to_owned());
    }
    let manifest_path = canonical_owned.join("ownership.json");
    let manifest: JobOwnership = serde_json::from_slice(
        &fs::read(&manifest_path)
            .map_err(|error| format!("native job ownership cannot be read: {error}"))?,
    )
    .map_err(|error| format!("native job ownership is invalid: {error}"))?;
    if manifest.job_id != job_id {
        return Err("native job ownership does not match cleanup target".to_owned());
    }
    fs::remove_dir_all(canonical_owned)
        .map_err(|error| format!("native job directory cannot be removed: {error}"))
}

struct PersistentReplacementSink {
    app: AppHandle,
}

impl restore::ReplacementSink for PersistentReplacementSink {
    fn begin(
        &self,
    ) -> crate::persistent_store::StoreResult<crate::persistent_store::StagingResult> {
        crate::persistent_store::commands::with_store_mut(
            self.app.state(),
            crate::persistent_store::PersistentStore::replace_begin,
        )
    }

    fn put_root(
        &self,
        staging_id: &str,
        root: &serde_json::Value,
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_root(staging_id, root)
        })
    }

    fn put_presets(
        &self,
        staging_id: &str,
        presets: &[serde_json::Value],
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_presets(staging_id, presets)
        })
    }

    fn add_characters(
        &self,
        staging_id: &str,
        characters: &[serde_json::Value],
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_add_characters(staging_id, characters)
        })
    }

    fn commit(
        &self,
        staging_id: &str,
        expected_revision: i64,
    ) -> crate::persistent_store::StoreResult<crate::persistent_store::RevisionResult> {
        crate::persistent_store::commands::replace_commit_with_snapshot(
            &self.app,
            staging_id,
            Some(expected_revision),
        )
    }

    fn abort(&self, staging_id: &str) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_abort(staging_id)
        })
    }
}

#[tauri::command(async)]
pub(crate) fn native_file_job_start(
    app: AppHandle,
    state: State<'_, NativeFileJobState>,
    request: NativeFileJobStartRequest,
) -> Result<NativeFileJobStarted, NativeJobError> {
    state.start(request, app)
}

#[tauri::command(async)]
pub(crate) fn native_file_job_status(
    state: State<'_, NativeFileJobState>,
    job_id: String,
) -> Result<JobStatus, NativeJobError> {
    state.status(&job_id)
}

#[tauri::command(async)]
pub(crate) fn native_file_job_list(
    state: State<'_, NativeFileJobState>,
) -> Result<Vec<JobStatus>, NativeJobError> {
    state.list()
}

#[tauri::command(async)]
pub(crate) fn native_file_job_finalize(
    state: State<'_, NativeFileJobState>,
    job_id: String,
) -> Result<FinalizeOutcome, NativeJobError> {
    state.finalize(&job_id)
}

#[tauri::command(async)]
pub(crate) fn native_file_job_cancel(
    state: State<'_, NativeFileJobState>,
    job_id: String,
) -> Result<CancelOutcome, NativeJobError> {
    state.cancel(&job_id)
}

#[tauri::command(async)]
pub(crate) fn native_file_job_forget(
    state: State<'_, NativeFileJobState>,
    job_id: String,
) -> Result<bool, NativeJobError> {
    state.forget(&job_id)
}

#[tauri::command(async)]
pub(crate) fn native_lossless_handoff_cleanup(
    state: State<'_, NativeFileJobState>,
    path: String,
) -> Result<bool, NativeJobError> {
    cleanup_lossless_handoff_path(&state.root, Path::new(&path))
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JobKind {
    RestoreBlockRisuSave,
    RestoreLosslessBackup,
    RestoreOfficialAccountSnapshot,
    ExportBlockRisuSave,
    ExportLosslessBackup,
    PrepareContentImport,
    KeiBackupUpload,
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum JobState {
    Queued,
    Running,
    WaitingForInput,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JobPhase {
    Queued,
    ReadingSource,
    AwaitingContentMapping,
    StagingDatabase,
    AwaitingActivation,
    ActivatingDatabase,
    WritingExport,
    PublishingDestination,
    FinalizingExport,
    Complete,
}

#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobProgress {
    pub(crate) completed_bytes: u64,
    pub(crate) total_bytes: Option<u64>,
    pub(crate) completed_items: u64,
    pub(crate) total_items: Option<u64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobStatus {
    pub(crate) job_id: String,
    pub(crate) kind: JobKind,
    pub(crate) state: JobState,
    pub(crate) phase: JobPhase,
    pub(crate) progress: JobProgress,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expected_revision: Option<i64>,
    pub(crate) warning_codes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<JobResultSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<JobFailure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) prepared_content: Option<PreparedContent>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobResultSummary {
    pub(crate) revision: i64,
    pub(crate) source_bytes: u64,
    pub(crate) source_sha256: String,
    pub(crate) character_count: u64,
    pub(crate) preset_count: u64,
    pub(crate) warning_codes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) handoff_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) recovery_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobFailure {
    pub(crate) code: String,
    pub(crate) message: String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CancelOutcome {
    Requested,
    AlreadyRequested,
    TooLate,
    Terminal,
    Missing,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum FinalizeOutcome {
    Requested,
    AlreadyRequested,
    TooEarly,
    Terminal,
    Missing,
}

pub(crate) struct JobRegistry {
    jobs: Mutex<HashMap<String, Arc<JobControl>>>,
    max_terminal_jobs: usize,
    max_terminal_age: Duration,
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::with_retention(64, Duration::from_secs(60 * 60))
    }
}

impl JobRegistry {
    fn with_retention(max_terminal_jobs: usize, max_terminal_age: Duration) -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            max_terminal_jobs,
            max_terminal_age,
        }
    }

    #[cfg(test)]
    pub(crate) fn create(&self, kind: JobKind) -> Result<Arc<JobControl>, String> {
        self.create_internal(kind, None, Vec::new(), false)
    }

    #[cfg(test)]
    pub(crate) fn create_with_context(
        &self,
        kind: JobKind,
        expected_revision: Option<i64>,
        warning_codes: Vec<String>,
    ) -> Result<Arc<JobControl>, String> {
        self.create_internal(
            kind,
            expected_revision,
            warning_codes,
            matches!(
                kind,
                JobKind::RestoreBlockRisuSave
                    | JobKind::RestoreLosslessBackup
                    | JobKind::RestoreOfficialAccountSnapshot
            ),
        )
    }

    fn create_internal(
        &self,
        kind: JobKind,
        expected_revision: Option<i64>,
        warning_codes: Vec<String>,
        requires_restore_finalization: bool,
    ) -> Result<Arc<JobControl>, String> {
        self.prune()?;
        validate_warning_codes(&warning_codes)?;
        let id = Uuid::new_v4().to_string();
        let job = Arc::new(JobControl {
            cancel_requested: AtomicBool::new(false),
            requires_restore_finalization,
            restore_finalized: Mutex::new(false),
            restore_finalization_changed: Condvar::new(),
            terminal_at: Mutex::new(None),
            status: Mutex::new(JobStatus {
                job_id: id.clone(),
                kind,
                state: JobState::Queued,
                phase: JobPhase::Queued,
                progress: JobProgress::default(),
                expected_revision,
                warning_codes,
                result: None,
                error: None,
                prepared_content: None,
            }),
        });
        self.jobs
            .lock()
            .map_err(|error| format!("native job registry mutex poisoned: {error}"))?
            .insert(id, Arc::clone(&job));
        Ok(job)
    }

    pub(crate) fn list(&self) -> Result<Vec<JobStatus>, String> {
        self.prune()?;
        let jobs = self
            .jobs
            .lock()
            .map_err(|error| format!("native job registry mutex poisoned: {error}"))?
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut statuses = jobs.into_iter().map(|job| job.status()).collect::<Vec<_>>();
        statuses.sort_by(|left, right| left.job_id.cmp(&right.job_id));
        Ok(statuses)
    }

    pub(crate) fn status(&self, id: &str) -> Result<JobStatus, String> {
        self.prune()?;
        let job = self
            .lookup(id)?
            .ok_or_else(|| "native job not found".to_owned())?;
        Ok(job.status())
    }

    pub(crate) fn cancel(&self, id: &str) -> Result<CancelOutcome, String> {
        self.prune()?;
        let Some(job) = self.lookup(id)? else {
            return Ok(CancelOutcome::Missing);
        };
        job.request_cancel()
    }

    pub(crate) fn finalize(&self, id: &str) -> Result<FinalizeOutcome, String> {
        self.prune()?;
        let Some(job) = self.lookup(id)? else {
            return Ok(FinalizeOutcome::Missing);
        };
        job.request_finalize()
    }

    pub(crate) fn forget(&self, id: &str) -> Result<bool, String> {
        self.prune()?;
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|error| format!("native job registry mutex poisoned: {error}"))?;
        let Some(job) = jobs.get(id) else {
            return Ok(false);
        };
        if !job.status().state.is_terminal() {
            return Err("native job cannot be forgotten before it is terminal".to_owned());
        }
        jobs.remove(id);
        Ok(true)
    }

    fn lookup(&self, id: &str) -> Result<Option<Arc<JobControl>>, String> {
        Ok(self
            .jobs
            .lock()
            .map_err(|error| format!("native job registry mutex poisoned: {error}"))?
            .get(id)
            .cloned())
    }

    fn prune(&self) -> Result<(), String> {
        let now = Instant::now();
        let mut jobs = self
            .jobs
            .lock()
            .map_err(|error| format!("native job registry mutex poisoned: {error}"))?;
        let mut terminal = Vec::new();
        for (id, job) in jobs.iter() {
            if job.retains_prepared_content_until_forget()? {
                continue;
            }
            if let Some(time) = job.terminal_time() {
                terminal.push((id.clone(), time));
            }
        }
        for (id, completed_at) in &terminal {
            if now.saturating_duration_since(*completed_at) >= self.max_terminal_age {
                jobs.remove(id);
            }
        }
        terminal.retain(|(id, _)| jobs.contains_key(id));
        terminal.sort_by_key(|(_, completed_at)| *completed_at);
        let excess = terminal.len().saturating_sub(self.max_terminal_jobs);
        for (id, _) in terminal.into_iter().take(excess) {
            jobs.remove(&id);
        }
        Ok(())
    }
}

pub(crate) struct JobControl {
    cancel_requested: AtomicBool,
    requires_restore_finalization: bool,
    restore_finalized: Mutex<bool>,
    restore_finalization_changed: Condvar,
    terminal_at: Mutex<Option<Instant>>,
    status: Mutex<JobStatus>,
}

impl JobControl {
    pub(crate) fn id(&self) -> String {
        self.status().job_id
    }

    pub(crate) fn status(&self) -> JobStatus {
        self.status
            .lock()
            .expect("job status mutex poisoned")
            .clone()
    }

    pub(crate) fn is_cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::Acquire)
    }

    fn retains_prepared_content_until_forget(&self) -> Result<bool, String> {
        let status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        Ok(status.kind == JobKind::PrepareContentImport
            && status.state == JobState::Succeeded
            && status.prepared_content.is_some())
    }

    fn request_cancel(&self) -> Result<CancelOutcome, String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state.is_terminal() {
            return Ok(CancelOutcome::Terminal);
        }
        if matches!(
            status.phase,
            JobPhase::ActivatingDatabase | JobPhase::FinalizingExport
        ) {
            return Ok(CancelOutcome::TooLate);
        }
        let wait = self
            .restore_finalized
            .lock()
            .map_err(|error| format!("native job wait mutex poisoned: {error}"))?;
        if self.cancel_requested.swap(true, Ordering::AcqRel) {
            return Ok(CancelOutcome::AlreadyRequested);
        }
        status.state = JobState::Cancelling;
        drop(status);
        drop(wait);
        self.restore_finalization_changed.notify_all();
        Ok(CancelOutcome::Requested)
    }

    fn request_finalize(&self) -> Result<FinalizeOutcome, String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state.is_terminal() {
            return Ok(FinalizeOutcome::Terminal);
        }
        if !matches!(
            status.kind,
            JobKind::RestoreBlockRisuSave
                | JobKind::RestoreLosslessBackup
                | JobKind::RestoreOfficialAccountSnapshot
        ) {
            return Ok(FinalizeOutcome::TooEarly);
        }
        if matches!(
            status.phase,
            JobPhase::ActivatingDatabase | JobPhase::Complete
        ) {
            return Ok(FinalizeOutcome::AlreadyRequested);
        }
        if status.state != JobState::WaitingForInput || status.phase != JobPhase::AwaitingActivation
        {
            return Ok(FinalizeOutcome::TooEarly);
        }
        if self.cancel_requested.load(Ordering::Acquire) {
            return Ok(FinalizeOutcome::TooEarly);
        }
        let mut finalized = self
            .restore_finalized
            .lock()
            .map_err(|error| format!("native restore finalization mutex poisoned: {error}"))?;
        if *finalized {
            return Ok(FinalizeOutcome::AlreadyRequested);
        }
        *finalized = true;
        status.state = JobState::Running;
        status.phase = JobPhase::ActivatingDatabase;
        drop(finalized);
        drop(status);
        self.restore_finalization_changed.notify_all();
        Ok(FinalizeOutcome::Requested)
    }

    pub(crate) fn wait_for_restore_finalization(&self) -> Result<(), String> {
        {
            let mut status = self
                .status
                .lock()
                .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
            if !matches!(
                status.kind,
                JobKind::RestoreBlockRisuSave
                    | JobKind::RestoreLosslessBackup
                    | JobKind::RestoreOfficialAccountSnapshot
            ) || status.state != JobState::Running
                || status.phase != JobPhase::StagingDatabase
            {
                return Err("native restore cannot activate from its current state".to_owned());
            }
            if self.cancel_requested.load(Ordering::Acquire) {
                return Err("native restore was cancelled before activation".to_owned());
            }
            if !self.requires_restore_finalization {
                status.phase = JobPhase::ActivatingDatabase;
                return Ok(());
            }
            status.state = JobState::WaitingForInput;
            status.phase = JobPhase::AwaitingActivation;
        }

        let mut finalized = self
            .restore_finalized
            .lock()
            .map_err(|error| format!("native restore finalization mutex poisoned: {error}"))?;
        loop {
            if self.cancel_requested.load(Ordering::Acquire) {
                return Err("native restore was cancelled before activation".to_owned());
            }
            if *finalized {
                return Ok(());
            }
            finalized = self
                .restore_finalization_changed
                .wait(finalized)
                .map_err(|error| format!("native restore finalization mutex poisoned: {error}"))?;
        }
    }

    pub(crate) fn start(&self, phase: JobPhase) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        let expected = match status.kind {
            JobKind::RestoreBlockRisuSave => JobPhase::ReadingSource,
            JobKind::RestoreLosslessBackup => JobPhase::ReadingSource,
            JobKind::RestoreOfficialAccountSnapshot => JobPhase::ReadingSource,
            JobKind::ExportBlockRisuSave => JobPhase::WritingExport,
            JobKind::ExportLosslessBackup => JobPhase::WritingExport,
            JobKind::PrepareContentImport => JobPhase::ReadingSource,
            JobKind::KeiBackupUpload => JobPhase::WritingExport,
        };
        if status.state != JobState::Queued || phase != expected {
            return Err("native job can only start from queued".to_owned());
        }
        status.state = JobState::Running;
        status.phase = phase;
        Ok(())
    }

    pub(crate) fn set_progress(&self, progress: JobProgress) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state != JobState::Running {
            return Err("native job progress requires a running job".to_owned());
        }
        if progress.completed_bytes < status.progress.completed_bytes
            || progress.completed_items < status.progress.completed_items
            || progress
                .total_bytes
                .is_some_and(|total| progress.completed_bytes > total)
            || progress
                .total_items
                .is_some_and(|total| progress.completed_items > total)
            || status
                .progress
                .total_bytes
                .is_some_and(|total| progress.total_bytes != Some(total))
            || status
                .progress
                .total_items
                .is_some_and(|total| progress.total_items != Some(total))
        {
            return Err("native job progress is invalid or non-monotonic".to_owned());
        }
        status.progress = progress;
        Ok(())
    }

    pub(crate) fn set_phase(&self, phase: JobPhase) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state != JobState::Running {
            return Err("native job phase requires a running job".to_owned());
        }
        if phase == JobPhase::Complete || phase.rank() != status.phase.rank() + 1 {
            return Err("native job phase transition is invalid".to_owned());
        }
        status.phase = phase;
        Ok(())
    }

    pub(crate) fn finish_cancelled(&self) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if !self.is_cancel_requested() || status.state.is_terminal() {
            return Err("native job cannot finish cancelled from its current state".to_owned());
        }
        status.state = JobState::Cancelled;
        status.phase = JobPhase::Complete;
        status.prepared_content = None;
        drop(status);
        self.mark_terminal()?;
        Ok(())
    }

    fn finish_content_job(
        &self,
        outcome: Result<PreparedContent, NativeJobError>,
        cleanup: Result<(), String>,
    ) -> Result<(), String> {
        self.finish_content_job_inner(outcome, cleanup, None)
    }

    fn finish_content_job_with_cas(
        &self,
        outcome: Result<PreparedContent, NativeJobError>,
        cleanup: Result<(), String>,
        repository_root: &Path,
    ) -> Result<(), String> {
        self.finish_content_job_inner(outcome, cleanup, Some(repository_root))
    }

    fn finish_content_job_inner(
        &self,
        outcome: Result<PreparedContent, NativeJobError>,
        cleanup: Result<(), String>,
        repository_root: Option<&Path>,
    ) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.kind != JobKind::PrepareContentImport || status.state.is_terminal() {
            return Err("native content job cannot finish from its current state".to_owned());
        }
        let cleanup = match repository_root {
            Some(repository_root) => finish_content_cas_session(
                repository_root,
                &status.job_id,
                self.is_cancel_requested(),
                &outcome,
                cleanup,
            ),
            None => cleanup,
        };
        let (state, failure, prepared_content) = match (outcome, cleanup) {
            (Err(error), Err(cleanup)) => (
                JobState::Failed,
                Some(JobFailure {
                    code: "cleanup-failed".to_owned(),
                    message: bounded_message(&format!(
                        "{}; cleanup failed: {cleanup}",
                        error.message
                    )),
                }),
                None,
            ),
            (Ok(_), Err(cleanup)) => (
                JobState::Failed,
                Some(JobFailure {
                    code: "cleanup-failed".to_owned(),
                    message: bounded_message(&cleanup),
                }),
                None,
            ),
            (_, Ok(())) if self.is_cancel_requested() => (JobState::Cancelled, None, None),
            (Err(error), Ok(())) => (
                JobState::Failed,
                Some(JobFailure {
                    code: error.code,
                    message: error.message,
                }),
                None,
            ),
            (Ok(prepared), Ok(())) => (JobState::Succeeded, None, Some(prepared)),
        };
        status.state = state;
        status.phase = JobPhase::Complete;
        status.result = None;
        status.error = failure;
        status.prepared_content = prepared_content;
        drop(status);
        self.mark_terminal()
    }

    pub(crate) fn finish_success(&self, mut result: JobResultSummary) -> Result<(), String> {
        validate_result(&result)?;
        let context_warnings = self.status().warning_codes;
        for warning in context_warnings {
            if !result.warning_codes.contains(&warning) {
                result.warning_codes.push(warning);
            }
        }
        result.warning_codes.truncate(MAX_WARNING_CODES);
        validate_result(&result)?;
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state != JobState::Running || self.is_cancel_requested() {
            return Err("native job cannot succeed from its current state".to_owned());
        }
        status.state = JobState::Succeeded;
        status.phase = JobPhase::Complete;
        status.result = Some(result);
        status.error = None;
        status.prepared_content = None;
        drop(status);
        self.mark_terminal()?;
        Ok(())
    }

    pub(crate) fn finish_failure(&self, code: &str, message: &str) -> Result<(), String> {
        if code.is_empty() || code.len() > MAX_CODE_BYTES {
            return Err("native job error code is invalid".to_owned());
        }
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state.is_terminal() {
            return Err("native job is already terminal".to_owned());
        }
        status.state = JobState::Failed;
        status.phase = JobPhase::Complete;
        status.result = None;
        status.error = Some(JobFailure {
            code: code.to_owned(),
            message: bounded_message(message),
        });
        status.prepared_content = None;
        drop(status);
        self.mark_terminal()?;
        Ok(())
    }

    fn mark_terminal(&self) -> Result<(), String> {
        *self
            .terminal_at
            .lock()
            .map_err(|error| format!("native job terminal mutex poisoned: {error}"))? =
            Some(Instant::now());
        Ok(())
    }

    fn terminal_time(&self) -> Option<Instant> {
        *self
            .terminal_at
            .lock()
            .expect("native job terminal mutex poisoned")
    }
}

fn validate_result(result: &JobResultSummary) -> Result<(), String> {
    validate_warning_codes(&result.warning_codes)?;
    if result.source_sha256.len() != 64
        || !result
            .source_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("native job result has an invalid source hash".to_owned());
    }
    Ok(())
}

fn validate_warning_codes(warning_codes: &[String]) -> Result<(), String> {
    if warning_codes.len() > MAX_WARNING_CODES {
        return Err("native job has too many warning codes".to_owned());
    }
    if warning_codes
        .iter()
        .any(|code| code.is_empty() || code.len() > MAX_CODE_BYTES)
    {
        return Err("native job has an invalid warning code".to_owned());
    }
    Ok(())
}

fn bounded_message(message: &str) -> String {
    let sanitized = message.replace(['\r', '\n'], " ");
    if sanitized.len() <= MAX_ERROR_MESSAGE_BYTES {
        return sanitized;
    }
    let mut end = MAX_ERROR_MESSAGE_BYTES;
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized[..end].to_owned()
}

impl JobState {
    fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

impl JobPhase {
    fn rank(self) -> u8 {
        match self {
            Self::Queued => 0,
            Self::ReadingSource | Self::WritingExport => 1,
            Self::AwaitingContentMapping | Self::StagingDatabase | Self::PublishingDestination => 2,
            Self::AwaitingActivation | Self::FinalizingExport => 3,
            Self::ActivatingDatabase => 4,
            Self::Complete => 5,
        }
    }
}

mod export;
pub(crate) mod restore;

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde_json::json;
    use std::fs;
    use std::io::{Cursor, Write};
    use std::sync::Barrier;
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;
    use zip::write::FileOptions;
    use zip::{CompressionMethod, ZipWriter};

    fn result(revision: i64) -> JobResultSummary {
        JobResultSummary {
            revision,
            source_bytes: 1,
            source_sha256: "a".repeat(64),
            character_count: 0,
            preset_count: 0,
            warning_codes: Vec::new(),
            handoff_path: None,
            recovery_path: None,
        }
    }

    #[test]
    fn lossless_job_requests_are_descriptor_only_and_keep_optional_native_paths_bounded() {
        let restore: NativeFileJobStartRequest = serde_json::from_value(json!({
            "kind": "restore-lossless-backup",
            "source": {
                "type": "desktopPath",
                "path": "C:\\chosen\\backup.risulossless"
            },
            "expectedRevision": 7
        }))
        .unwrap();
        assert!(matches!(
            restore,
            NativeFileJobStartRequest::RestoreLosslessBackup {
                source: JobSource::DesktopPath { .. },
                expected_revision: 7,
            }
        ));

        let export: NativeFileJobStartRequest = serde_json::from_value(json!({
            "kind": "export-lossless-backup",
            "expectedRevision": 8
        }))
        .unwrap();
        assert!(matches!(
            export,
            NativeFileJobStartRequest::ExportLosslessBackup {
                destination: None,
                expected_revision: 8,
            }
        ));

        let mut summary = result(9);
        summary.handoff_path =
            Some("C:\\app\\persistent\\exports\\risusave-123.risudat".to_owned());
        summary.recovery_path =
            Some("C:\\app\\persistent\\recovery\\lossless-123.risudat".to_owned());
        let encoded = serde_json::to_value(summary).unwrap();
        assert_eq!(
            encoded["handoffPath"],
            json!("C:\\app\\persistent\\exports\\risusave-123.risudat")
        );
        assert_eq!(
            encoded["recoveryPath"],
            json!("C:\\app\\persistent\\recovery\\lossless-123.risudat")
        );
        assert!(encoded.get("bytes").is_none());
    }

    #[test]
    fn lossless_restore_reuses_the_existing_finalize_and_too_late_cancel_boundary() {
        let registry = JobRegistry::default();
        let job = registry
            .create_with_context(JobKind::RestoreLosslessBackup, Some(4), Vec::new())
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        job.set_phase(JobPhase::StagingDatabase).unwrap();

        let waiter = Arc::clone(&job);
        let waited = std::thread::spawn(move || waiter.wait_for_restore_finalization());
        for _ in 0..100 {
            if job.status().phase == JobPhase::AwaitingActivation {
                break;
            }
            std::thread::yield_now();
        }
        assert_eq!(job.status().state, JobState::WaitingForInput);
        assert_eq!(job.status().phase, JobPhase::AwaitingActivation);
        assert_eq!(job.request_finalize().unwrap(), FinalizeOutcome::Requested);
        assert_eq!(waited.join().unwrap(), Ok(()));
        assert_eq!(job.status().phase, JobPhase::ActivatingDatabase);
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::TooLate);
        job.finish_success(result(5)).unwrap();
    }

    fn write_literal_spool(root: &Path, token: &str, manifest: &str, created_at_millis: u64) {
        let spool = root.join("sources").join(token);
        fs::create_dir_all(&spool).unwrap();
        fs::write(spool.join("source.risudat"), b"RISUSAVE\0").unwrap();
        fs::write(
            spool.join("ownership.json"),
            format!(
                "{{\"format\":\"risunest-android-saf-spool\",\"version\":1,\"token\":\"{token}\",\"createdAtMillis\":{created_at_millis}}}"
            ),
        )
        .unwrap();
        fs::write(spool.join("source.json"), manifest).unwrap();
    }

    fn encoded_risum_overlay() -> Vec<u8> {
        const RPACK_MAP: &[u8; 512] = include_bytes!("../../../src/ts/rpack/rpack_map.bin");
        let metadata = br#"{"type":"risuModule","module":{"trigger":[{"comment":"native trigger"}],"regex":[{"comment":"native regex"}],"lorebook":[{"comment":"native lore"}],"assets":[]}}"#;
        let encoded = metadata
            .iter()
            .map(|byte| RPACK_MAP[*byte as usize])
            .collect::<Vec<_>>();
        let mut risum = vec![111, 0];
        risum.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
        risum.extend_from_slice(&encoded);
        risum.push(0);
        risum
    }

    fn charx_fixture(appended_jpeg_prefix: Option<&[u8]>) -> Vec<u8> {
        let card = br#"{
            "spec":"chara_card_v3",
            "data":{
                "name":"Prepared CharX",
                "extensions":{},
                "assets":[
                    {"type":"icon","uri":"embeded://images/portrait.JPEG","name":"portrait","ext":"JPEG"},
                    {"type":"x-risu-asset","uri":"__asset:assets/config.JSON","name":"config","ext":"JSON"},
                    {"type":"emotion","uri":"embeded://images/portrait.JPEG","name":"portrait duplicate","ext":"JPEG"},
                    {"type":"x-risu-asset","uri":"data:image/png;base64,AQIDBA==","name":"inline","ext":"png"}
                ]
            }
        }"#;
        let risum = encoded_risum_overlay();
        let entries: [(&str, &[u8]); 5] = [
            ("card.json", card),
            ("images/portrait.JPEG", b"\xff\xd8\xff\xd9"),
            ("assets/config.JSON", br#"{"mode":"strict"}"#),
            ("module.risum", &risum),
            ("unused.bin", b"unreferenced payload"),
        ];
        let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            archive
                .start_file(
                    name,
                    FileOptions::default().compression_method(CompressionMethod::Stored),
                )
                .unwrap();
            archive.write_all(bytes).unwrap();
        }
        let mut output = appended_jpeg_prefix.unwrap_or(&[]).to_vec();
        output.extend_from_slice(&archive.finish().unwrap().into_inner());
        output
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = Vec::with_capacity(data.len() + 12);
        chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(data);
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        chunk.extend_from_slice(&crc.finalize().to_be_bytes());
        chunk
    }

    fn png_text_chunk(keyword: &str, value: &str) -> Vec<u8> {
        let mut data = keyword.as_bytes().to_vec();
        data.push(0);
        data.extend_from_slice(value.as_bytes());
        png_chunk(b"tEXt", &data)
    }

    fn png_card_fixture(
        chara_json: &str,
        ccv3_json: &str,
        embedded_assets: &[(&str, &[u8])],
    ) -> (Vec<u8>, Vec<u8>) {
        let signature = b"\x89PNG\r\n\x1a\n";
        let ihdr = png_chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        let preserved_text = png_text_chunk("comment", "preserved");
        let idat = png_chunk(b"IDAT", &[]);
        let iend = png_chunk(b"IEND", &[]);
        let mut source = signature.to_vec();
        source.extend_from_slice(&ihdr);
        source.extend_from_slice(&png_text_chunk("chara", &STANDARD.encode(chara_json)));
        source.extend_from_slice(&png_text_chunk("ccv3", &STANDARD.encode(ccv3_json)));
        source.extend_from_slice(&preserved_text);
        for (reference, bytes) in embedded_assets {
            source.extend_from_slice(&png_text_chunk(
                &format!("chara-ext-asset_:{reference}"),
                &STANDARD.encode(bytes),
            ));
        }
        source.extend_from_slice(&idat);
        source.extend_from_slice(&iend);

        let mut base = signature.to_vec();
        base.extend_from_slice(&ihdr);
        base.extend_from_slice(&preserved_text);
        base.extend_from_slice(&idat);
        base.extend_from_slice(&iend);
        (source, base)
    }

    fn wait_for_content_job(state: &NativeFileJobState, job_id: &str) -> JobStatus {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = state.status(job_id).unwrap();
            if status.state.is_terminal() {
                return status;
            }
            assert!(Instant::now() < deadline, "content preparation timed out");
            thread::yield_now();
        }
    }

    fn start_test_content_job(
        state: &NativeFileJobState,
        source: &Path,
        display_name: &str,
    ) -> NativeFileJobStarted {
        state
            .start_content_for_test(NativeFileJobStartRequest::PrepareContentImport {
                source: JobSource::DesktopPath {
                    path: source.to_string_lossy().into_owned(),
                },
                display_name: display_name.to_owned(),
            })
            .expect("start content preparation")
    }

    #[test]
    fn opened_source_spooling_copies_the_open_handle_bytes() {
        let directory = TempDir::new().unwrap();
        let source_path = directory.path().join("selected.charx");
        let original = b"opened-handle-content";
        fs::write(&source_path, original).unwrap();
        let mut source = open_regular_file_no_follow(&source_path).unwrap();
        let spool_path = directory.path().join("job-source.charx");

        content::spool_opened_source(&mut source, &spool_path, &|| false).unwrap();

        assert_eq!(fs::read(spool_path).unwrap(), original);
    }

    #[test]
    fn opened_source_spooling_rejects_early_eof_without_a_partial_spool() {
        let directory = TempDir::new().unwrap();
        let source_path = directory.path().join("truncated.charx");
        fs::write(&source_path, b"short").unwrap();
        let mut source = open_regular_file_no_follow(&source_path).unwrap();
        source.total_bytes += 1;
        let spool_path = directory.path().join("job-source.charx");

        let error = content::spool_opened_source(&mut source, &spool_path, &|| false)
            .expect_err("source ending early must be rejected");

        assert_eq!(error.code, "invalid-source");
        assert!(!spool_path.exists());
    }

    #[test]
    fn opened_source_spooling_rejects_growth_without_a_partial_spool() {
        let directory = TempDir::new().unwrap();
        let source_path = directory.path().join("growing.charx");
        fs::write(&source_path, b"longer").unwrap();
        let mut source = open_regular_file_no_follow(&source_path).unwrap();
        source.total_bytes -= 1;
        let spool_path = directory.path().join("job-source.charx");

        let error = content::spool_opened_source(&mut source, &spool_path, &|| false)
            .expect_err("source growth must be rejected");

        assert_eq!(error.code, "invalid-source");
        assert!(!spool_path.exists());
    }

    #[test]
    fn cancelled_opened_source_spooling_removes_the_partial_copy() {
        let directory = TempDir::new().unwrap();
        let source_path = directory.path().join("cancelled.charx");
        fs::write(&source_path, b"cancelled").unwrap();
        let mut source = open_regular_file_no_follow(&source_path).unwrap();
        let spool_path = directory.path().join("job-source.charx");

        let error = content::spool_opened_source(&mut source, &spool_path, &|| true)
            .expect_err("cancelled spooling must fail");

        assert_eq!(error.code, "cancelled");
        assert!(!spool_path.exists());
    }

    #[test]
    fn charx_spooling_rejects_raw_sources_over_the_derived_container_cap_before_creation() {
        let directory = TempDir::new().unwrap();
        let source_path = directory.path().join("oversized.charx");
        fs::write(&source_path, b"small fixture").unwrap();
        let mut source = open_regular_file_no_follow(&source_path).unwrap();
        let raw_cap = content::max_charx_spool_bytes();
        assert!(raw_cap > charx::CharXLimits::default().max_total_decoded_bytes);
        source.total_bytes = raw_cap + 1;
        let spool_path = directory.path().join("job-source.charx");

        let error = content::spool_charx_source(&mut source, &spool_path, &|| false)
            .expect_err("raw CharX sources over the container cap must be rejected");

        assert_eq!(error.code, "invalid-input");
        assert!(!spool_path.exists());
    }

    #[test]
    fn kotlin_camel_case_ready_manifest_is_claimed_once() {
        let directory = TempDir::new().unwrap();
        let token = Uuid::new_v4().to_string();
        write_literal_spool(
            directory.path(),
            &token,
            &format!(
                "{{\"token\":\"{token}\",\"state\":\"ready\",\"displayName\":\"database.risudat\",\"bytes\":9,\"totalBytes\":12}}"
            ),
            1,
        );
        let jobs_root = directory.path().join("jobs");
        fs::create_dir_all(&jobs_root).unwrap();
        let job_id = Uuid::new_v4().to_string();
        let owned = create_owned_directory(&jobs_root, &job_id).unwrap();

        let source = claim_spool_source(directory.path(), &token, &owned).unwrap();

        assert_eq!(source.file_name().unwrap(), "source.risudat");
        assert!(source.starts_with(owned.canonicalize().unwrap()));
        assert!(!directory.path().join("sources").join(&token).exists());
        assert!(claim_spool_source(directory.path(), &token, &owned).is_err());
    }

    #[test]
    fn ready_manifest_requires_a_non_null_exact_length() {
        for bytes in ["null", "8", "10"] {
            let directory = TempDir::new().unwrap();
            let token = Uuid::new_v4().to_string();
            write_literal_spool(
                directory.path(),
                &token,
                &format!(
                    "{{\"token\":\"{token}\",\"state\":\"ready\",\"displayName\":\"database.risudat\",\"bytes\":{bytes},\"totalBytes\":null}}"
                ),
                1,
            );
            let jobs_root = directory.path().join("jobs");
            fs::create_dir_all(&jobs_root).unwrap();
            let job_id = Uuid::new_v4().to_string();
            let owned = create_owned_directory(&jobs_root, &job_id).unwrap();

            assert!(claim_spool_source(directory.path(), &token, &owned).is_err());
        }
    }

    #[test]
    fn android_spool_tokens_require_canonical_rfc4122_uuid_v4() {
        assert!(parse_android_spool_token("99999999-9999-4999-8999-999999999999").is_ok());
        assert!(parse_android_spool_token("99999999-9999-1999-8999-999999999999").is_err());
        assert!(parse_android_spool_token("99999999-9999-4999-0999-999999999999").is_err());
        assert!(parse_android_spool_token("99999999-9999-4999-8999-99999999999A").is_err());
    }

    #[test]
    fn concurrent_claims_have_exactly_one_job_owned_winner() {
        let directory = TempDir::new().unwrap();
        let token = Uuid::new_v4().to_string();
        write_literal_spool(
            directory.path(),
            &token,
            &format!(
                "{{\"token\":\"{token}\",\"state\":\"ready\",\"displayName\":\"database.risudat\",\"bytes\":9,\"totalBytes\":null}}"
            ),
            1,
        );
        let jobs_root = directory.path().join("jobs");
        fs::create_dir_all(&jobs_root).unwrap();
        let owned = [Uuid::new_v4(), Uuid::new_v4()]
            .map(|id| create_owned_directory(&jobs_root, &id.to_string()).unwrap());
        let barrier = Arc::new(Barrier::new(2));
        let handles = owned.map(|owned_directory| {
            let root = directory.path().to_owned();
            let token = token.clone();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                claim_spool_source(&root, &token, &owned_directory)
            })
        });

        let outcomes = handles.map(|handle| handle.join().unwrap());

        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_err()).count(),
            1
        );
    }

    #[test]
    fn stale_cleanup_and_job_claim_compete_by_atomic_directory_rename() {
        for _ in 0..32 {
            let directory = TempDir::new().unwrap();
            let token = Uuid::new_v4().to_string();
            write_literal_spool(
                directory.path(),
                &token,
                &format!(
                    "{{\"token\":\"{token}\",\"state\":\"ready\",\"displayName\":\"database.risudat\",\"bytes\":9,\"totalBytes\":null}}"
                ),
                1,
            );
            let jobs_root = directory.path().join("jobs");
            fs::create_dir_all(&jobs_root).unwrap();
            let owned = create_owned_directory(&jobs_root, &Uuid::new_v4().to_string()).unwrap();
            let barrier = Arc::new(Barrier::new(2));
            let claim_root = directory.path().to_owned();
            let claim_token = token.clone();
            let claim_owned = owned.clone();
            let claim_barrier = Arc::clone(&barrier);
            let claim = thread::spawn(move || {
                claim_barrier.wait();
                claim_spool_source(&claim_root, &claim_token, &claim_owned)
            });
            let cleanup_root = directory.path().join("sources");
            let cleanup_barrier = Arc::clone(&barrier);
            let cleanup = thread::spawn(move || {
                cleanup_barrier.wait();
                cleanup_spool_directories_at(&cleanup_root, 2_000, 100)
            });

            let claimed = claim.join().unwrap();
            cleanup.join().unwrap().unwrap();

            match claimed {
                Ok(source) => assert_eq!(fs::read(source).unwrap(), b"RISUSAVE\0"),
                Err(_) => assert!(!owned.join("android-source").exists()),
            }
            assert!(!directory.path().join("sources").join(&token).exists());
        }
    }

    #[test]
    fn startup_cleanup_uses_stable_owner_and_preserves_fresh_spools() {
        let directory = TempDir::new().unwrap();
        let sources = directory.path().join("sources");
        let stale = Uuid::new_v4().to_string();
        let fresh = Uuid::new_v4().to_string();
        write_literal_spool(directory.path(), &stale, "{\"token\":", 1);
        write_literal_spool(directory.path(), &fresh, "{\"token\":", 1_950);

        cleanup_spool_directories_at(&sources, 2_000, 100).unwrap();

        assert!(!sources.join(stale).exists());
        assert!(sources.join(fresh).exists());
    }

    #[test]
    fn startup_cleanup_preserves_a_conflicting_tombstone_without_matching_stable_owner() {
        let directory = TempDir::new().unwrap();
        let sources = directory.path().join("sources");
        let token = Uuid::new_v4().to_string();
        let foreign_token = Uuid::new_v4().to_string();
        write_literal_spool(directory.path(), &token, "{\"token\":", 1);
        let tombstone = sources.join(format!("{ANDROID_SPOOL_CLEANUP_PREFIX}{token}"));
        fs::create_dir_all(&tombstone).unwrap();
        fs::write(
            tombstone.join("ownership.json"),
            serde_json::to_vec(&SpoolOwnership {
                format: ANDROID_SPOOL_FORMAT.to_owned(),
                version: ANDROID_SPOOL_VERSION,
                token: foreign_token,
                created_at_millis: 1,
            })
            .unwrap(),
        )
        .unwrap();
        let sentinel = tombstone.join("source.risudat");
        fs::write(&sentinel, b"preserve-me").unwrap();

        cleanup_spool_directories_at(&sources, 2_000, 100).unwrap();

        assert!(sources.join(&token).is_dir());
        assert_eq!(fs::read(sentinel).unwrap(), b"preserve-me");
    }

    #[test]
    fn concurrent_cleanup_sweepers_treat_a_removed_owned_tombstone_as_a_lost_race() {
        let directory = TempDir::new().unwrap();
        let sources = directory.path().join("sources");
        fs::create_dir_all(&sources).unwrap();
        let token = Uuid::new_v4().to_string();
        let tombstone = sources.join(format!("{ANDROID_SPOOL_CLEANUP_PREFIX}{token}"));
        fs::create_dir_all(&tombstone).unwrap();
        fs::write(
            tombstone.join("ownership.json"),
            serde_json::to_vec(&SpoolOwnership {
                format: ANDROID_SPOOL_FORMAT.to_owned(),
                version: ANDROID_SPOOL_VERSION,
                token,
                created_at_millis: 1,
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(tombstone.join("source.risudat"), b"stale").unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let sweepers = (0..2)
            .map(|_| {
                let sources = sources.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    cleanup_spool_directories_at(&sources, 2_000, 100)
                })
            })
            .collect::<Vec<_>>();

        for sweeper in sweepers {
            sweeper.join().unwrap().unwrap();
        }
        assert!(!tombstone.exists());
    }

    #[test]
    fn permission_denied_is_a_race_loss_only_after_the_path_disappears() {
        let directory = TempDir::new().unwrap();
        let permission_denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(!is_spool_cleanup_race_loss(
            &permission_denied,
            directory.path(),
        ));

        let missing = directory.path().join("already-moved");
        assert_eq!(
            is_spool_cleanup_race_loss(&permission_denied, &missing),
            cfg!(windows),
        );
        assert!(is_spool_cleanup_race_loss(
            &std::io::Error::from(std::io::ErrorKind::NotFound),
            directory.path(),
        ));
    }

    #[test]
    fn job_status_tracks_only_legal_monotonic_progress() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();

        assert_eq!(job.status().state, JobState::Queued);
        job.start(JobPhase::ReadingSource).unwrap();
        job.set_progress(JobProgress {
            completed_bytes: 8,
            total_bytes: Some(16),
            completed_items: 1,
            total_items: None,
        })
        .unwrap();
        job.set_progress(JobProgress {
            completed_bytes: 7,
            total_bytes: Some(16),
            completed_items: 1,
            total_items: None,
        })
        .unwrap_err();

        let status = registry.status(&job.id()).unwrap();
        assert_eq!(status.kind, JobKind::RestoreBlockRisuSave);
        assert_eq!(status.state, JobState::Running);
        assert_eq!(status.phase, JobPhase::ReadingSource);
        assert_eq!(status.progress.completed_bytes, 8);
    }

    #[test]
    fn registry_lists_active_and_terminal_jobs_without_a_known_job_id() {
        let registry = JobRegistry::default();
        let active = registry
            .create_with_context(
                JobKind::RestoreBlockRisuSave,
                Some(7),
                vec!["cleanup-failed".to_owned()],
            )
            .unwrap();
        active.start(JobPhase::ReadingSource).unwrap();
        let terminal = registry
            .create_with_context(JobKind::ExportBlockRisuSave, Some(8), Vec::new())
            .unwrap();
        terminal.start(JobPhase::WritingExport).unwrap();
        terminal.finish_success(result(8)).unwrap();

        let listed = registry.list().unwrap();

        assert_eq!(listed.len(), 2);
        let mut expected_ids = vec![active.id(), terminal.id()];
        expected_ids.sort();
        assert_eq!(
            listed
                .iter()
                .map(|status| status.job_id.clone())
                .collect::<Vec<_>>(),
            expected_ids,
        );
        let restore = listed
            .iter()
            .find(|status| status.job_id == active.id())
            .unwrap();
        assert_eq!(restore.expected_revision, Some(7));
        assert_eq!(restore.warning_codes, vec!["cleanup-failed"]);
        assert_eq!(restore.state, JobState::Running);
        assert_eq!(
            listed
                .iter()
                .find(|status| status.job_id == terminal.id())
                .unwrap()
                .state,
            JobState::Succeeded,
        );
    }

    #[test]
    fn restore_waits_for_explicit_finalize_and_cancel_wakes_the_waiter() {
        let registry = Arc::new(JobRegistry::default());
        let job = registry
            .create_with_context(JobKind::RestoreBlockRisuSave, Some(3), Vec::new())
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        job.set_phase(JobPhase::StagingDatabase).unwrap();
        let waiter = Arc::clone(&job);
        let waited = std::thread::spawn(move || waiter.wait_for_restore_finalization());

        while job.status().state != JobState::WaitingForInput {
            std::thread::yield_now();
        }
        assert_eq!(job.status().phase, JobPhase::AwaitingActivation);
        assert_eq!(
            registry.finalize(&job.id()).unwrap(),
            FinalizeOutcome::Requested
        );
        assert_eq!(waited.join().unwrap(), Ok(()));
        assert_eq!(job.status().state, JobState::Running);
        assert_eq!(job.status().phase, JobPhase::ActivatingDatabase);

        let cancelled = registry
            .create_with_context(JobKind::RestoreBlockRisuSave, Some(4), Vec::new())
            .unwrap();
        cancelled.start(JobPhase::ReadingSource).unwrap();
        cancelled.set_phase(JobPhase::StagingDatabase).unwrap();
        let waiter = Arc::clone(&cancelled);
        let waited = std::thread::spawn(move || waiter.wait_for_restore_finalization());
        while cancelled.status().state != JobState::WaitingForInput {
            std::thread::yield_now();
        }
        assert_eq!(
            registry.cancel(&cancelled.id()).unwrap(),
            CancelOutcome::Requested
        );
        assert!(waited.join().unwrap().is_err());
    }

    #[test]
    fn content_prepare_promotes_assets_and_succeeds_with_logical_descriptors() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("card.json");
        fs::write(
            &source,
            br#"{"spec":"chara_card_v3","data":{"name":"Prepared","assets":[{"type":"icon","uri":"data:image/png;base64,AQIDBA==","name":"main","ext":"png"}]}}"#,
        )
        .unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = state
            .start_content_for_test(NativeFileJobStartRequest::PrepareContentImport {
                source: JobSource::DesktopPath {
                    path: source.to_string_lossy().into_owned(),
                },
                display_name: "card.json".to_owned(),
            })
            .expect("start content preparation");

        let deadline = Instant::now() + Duration::from_secs(5);
        let prepared = loop {
            let status = state.status(&started.job_id).unwrap();
            if status.state.is_terminal() {
                break status;
            }
            assert!(Instant::now() < deadline, "content preparation timed out");
            thread::yield_now();
        };
        assert_eq!(prepared.kind, JobKind::PrepareContentImport);
        assert_eq!(prepared.state, JobState::Succeeded);
        assert_eq!(prepared.phase, JobPhase::Complete);
        let content = prepared
            .prepared_content
            .as_ref()
            .expect("prepared content metadata");
        assert_eq!(content.format, PreparedContentFormat::JsonCard);
        assert_eq!(content.cas_session_id, started.job_id);
        assert_eq!(content.cas_session_id.len(), 36);
        assert_eq!(
            serde_json::to_value(content)
                .unwrap()
                .pointer("/casSessionId")
                .and_then(Value::as_str),
            Some(started.job_id.as_str())
        );
        assert_eq!(content.assets.len(), 1);
        assert_eq!(content.assets[0].byte_size, 4);
        assert_eq!(content.assets[0].reference_key, "native-data-0");
        assert_eq!(content.assets[0].token, "native-data-0");
        assert_eq!(
            content.assets[0].object_hash,
            "9f64a747e1b97f131fabb6b447296c9b6f0201e79fb3c5356e6c77e89b6a806a"
        );
        assert_eq!(
            content.assets[0].logical_id,
            "assets/9f64a747e1b97f131fabb6b447296c9b6f0201e79fb3c5356e6c77e89b6a806a.png"
        );
        assert_eq!(content.assets[0].mime, "image/png");
        assert_eq!(content.assets[0].name, "main");
        assert_eq!(content.assets[0].ext, "png");
        assert_eq!(
            content
                .metadata
                .pointer("/data/name")
                .and_then(Value::as_str),
            Some("Prepared")
        );
        let encoded = serde_json::to_string(&prepared).unwrap();
        assert!(!encoded.contains("stagedPath"));
        assert!(!encoded.contains(".payload"));
        assert!(!encoded.contains("AQIDBA"));
        assert!(!directory
            .path()
            .join("native-file-jobs/jobs")
            .join(&started.job_id)
            .exists());
        assert_eq!(state.active_workers.load(Ordering::Acquire), 0);
        let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
        assert_eq!(
            cas.read_object(&content.assets[0].object_hash).unwrap(),
            Some(vec![1, 2, 3, 4])
        );
        let session = crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &started.job_id,
        )
        .expect("prepared content keeps its durable CAS session for TypeScript activation");
        assert_eq!(
            session.kind(),
            crate::asset_repository::job_pins::CasJobKind::CardOrModuleContentImport
        );
        assert_eq!(session.pin_count(), 0);
        assert!(!session.is_sealed());
        assert!(!session.is_released());
        assert!(
            crate::asset_repository::job_pins::collect_durable_cas_job_roots(directory.path(),)
                .blockers
                .contains(&format!("job-pin-unsealed:{}", started.job_id))
        );

        assert_eq!(
            state.cancel(&started.job_id).unwrap(),
            CancelOutcome::Terminal
        );
        assert!(state.forget(&started.job_id).unwrap());
        assert!(state.status(&started.job_id).is_err());
    }

    #[test]
    fn content_prepare_charx_promotes_only_referenced_assets_with_distinct_tokens() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("prepared.charx");
        fs::write(&source, charx_fixture(None)).unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = start_test_content_job(&state, &source, "prepared.charx");

        let prepared = wait_for_content_job(&state, &started.job_id);

        assert_eq!(prepared.state, JobState::Succeeded);
        let content = prepared
            .prepared_content
            .as_ref()
            .expect("prepared CharX content");
        assert_eq!(content.cas_session_id, started.job_id);
        let serialized = serde_json::to_value(content).unwrap();
        assert_eq!(
            serialized.pointer("/format").and_then(Value::as_str),
            Some("charx-card")
        );
        assert_eq!(content.assets.len(), 4);
        assert_eq!(
            content
                .metadata
                .pointer("/data/assets/0/uri")
                .and_then(Value::as_str),
            Some("__asset:native-charx-0")
        );
        assert_eq!(
            content
                .metadata
                .pointer("/data/assets/1/uri")
                .and_then(Value::as_str),
            Some("__asset:native-charx-1")
        );
        assert_eq!(
            content
                .metadata
                .pointer("/data/assets/2/uri")
                .and_then(Value::as_str),
            Some("__asset:native-charx-2")
        );
        assert_eq!(
            content
                .metadata
                .pointer("/data/assets/3/uri")
                .and_then(Value::as_str),
            Some("__asset:native-data-3")
        );

        let first_portrait = content
            .assets
            .iter()
            .find(|asset| asset.token == "native-charx-0")
            .expect("first portrait occurrence");
        let config = content
            .assets
            .iter()
            .find(|asset| asset.token == "native-charx-1")
            .expect("config occurrence");
        let duplicate_portrait = content
            .assets
            .iter()
            .find(|asset| asset.token == "native-charx-2")
            .expect("second portrait occurrence");
        let inline = content
            .assets
            .iter()
            .find(|asset| asset.token == "native-data-3")
            .expect("data URI occurrence");
        assert_eq!(first_portrait.reference_key, "native-charx-0");
        assert_eq!(duplicate_portrait.reference_key, "native-charx-2");
        assert_eq!(first_portrait.object_hash, duplicate_portrait.object_hash);
        assert_eq!(first_portrait.logical_id, duplicate_portrait.logical_id);
        assert!(first_portrait.logical_id.ends_with(".jpeg"));
        assert_eq!(first_portrait.ext, "JPEG");
        assert!(config.logical_id.ends_with(".json"));
        assert_eq!(config.ext, "JSON");
        assert_eq!(config.mime, "application/json");
        assert_eq!(inline.ext, "png");
        assert_eq!(inline.mime, "image/png");
        assert_eq!(inline.byte_size, 4);
        assert_eq!(
            serialized
                .pointer("/module/trigger/0/comment")
                .and_then(Value::as_str),
            Some("native trigger")
        );
        assert_eq!(
            serialized
                .pointer("/module/regex/0/comment")
                .and_then(Value::as_str),
            Some("native regex")
        );
        assert_eq!(
            serialized
                .pointer("/module/lorebook/0/comment")
                .and_then(Value::as_str),
            Some("native lore")
        );
        let serialized_text = serde_json::to_string(content).unwrap();
        assert!(!serialized_text.contains("unreferenced payload"));
        assert!(!serialized_text.contains("stagedPath"));
        assert!(!serialized_text.contains("module.risum"));

        let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
        assert_eq!(
            cas.read_object(&first_portrait.object_hash).unwrap(),
            Some(vec![0xff, 0xd8, 0xff, 0xd9])
        );
        assert_eq!(
            cas.read_object(&config.object_hash).unwrap(),
            Some(br#"{"mode":"strict"}"#.to_vec())
        );
        assert_eq!(
            cas.read_object(&inline.object_hash).unwrap(),
            Some(vec![1, 2, 3, 4])
        );
        use sha2::{Digest as _, Sha256};
        let unused_hash = hex::encode(Sha256::digest(b"unreferenced payload"));
        assert_eq!(cas.stat_object(&unused_hash).unwrap(), None);
        let session = crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &started.job_id,
        )
        .expect("prepared CharX keeps its durable CAS session");
        assert_eq!(session.pin_count(), 0);
        assert!(!session.is_sealed());
        assert!(!session.is_released());
        assert!(!directory
            .path()
            .join("native-file-jobs/jobs")
            .join(&started.job_id)
            .exists());
    }

    #[test]
    fn content_prepare_png_preserves_encoded_metadata_and_promotes_exact_assets() {
        let directory = TempDir::new().unwrap();
        let chara = r#"{"spec":"chara_card_v2","data":{"name":"stale"}}"#;
        let ccv3 = r#"{"spec":"chara_card_v3","data":{"name":"PNG fixture"}}"#;
        let portrait_collision: &[u8] = b"embedded portrait token";
        let second: &[u8] = b"opaque ordinary asset";
        let (source_bytes, expected_base) = png_card_fixture(
            chara,
            ccv3,
            &[
                ("native-png-portrait", portrait_collision),
                ("007", second),
                ("007", second),
            ],
        );
        let source = directory.path().join("prepared.PNG");
        fs::write(&source, source_bytes).unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = start_test_content_job(&state, &source, "prepared.PNG");

        let prepared = wait_for_content_job(&state, &started.job_id);

        assert_eq!(prepared.state, JobState::Succeeded);
        let content = prepared
            .prepared_content
            .as_ref()
            .expect("prepared PNG content");
        assert_eq!(content.format, PreparedContentFormat::PngCard);
        assert_eq!(content.cas_session_id, started.job_id);
        assert_eq!(
            content.metadata,
            json!({ "chara": STANDARD.encode(chara), "ccv3": STANDARD.encode(ccv3) })
        );
        assert_eq!(content.assets.len(), 3);

        let portrait = &content.assets[0];
        assert_eq!(portrait.token, "native-png-portrait-1");
        assert_eq!(portrait.reference_key, portrait.token);
        assert_eq!(portrait.mime, "image/png");
        assert_eq!(portrait.ext, "png");
        assert_eq!(portrait.name, format!("{}.png", portrait.object_hash));
        assert_eq!(
            content.portrait_logical_id.as_deref(),
            Some(portrait.logical_id.as_str())
        );

        let collision = &content.assets[1];
        assert_eq!(collision.token, "native-png-portrait");
        assert_eq!(collision.reference_key, collision.token);
        assert_eq!(collision.mime, "");
        assert_eq!(collision.ext, "png");
        assert_eq!(collision.name, format!("{}.png", collision.object_hash));

        let numbered = &content.assets[2];
        assert_eq!(numbered.token, "007");
        assert_eq!(numbered.reference_key, numbered.token);
        assert_eq!(numbered.mime, "");
        assert_eq!(numbered.ext, "png");
        assert_eq!(numbered.name, format!("{}.png", numbered.object_hash));

        let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
        assert_eq!(
            cas.read_object(&portrait.object_hash).unwrap(),
            Some(expected_base)
        );
        assert_eq!(
            cas.read_object(&collision.object_hash).unwrap(),
            Some(portrait_collision.to_vec())
        );
        assert_eq!(
            cas.read_object(&numbered.object_hash).unwrap(),
            Some(second.to_vec())
        );
        let session = crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &started.job_id,
        )
        .expect("prepared PNG keeps its durable CAS session");
        assert_eq!(session.pin_count(), 0);
        assert!(!session.is_sealed());
        assert!(!session.is_released());
    }

    #[test]
    fn content_prepare_png_conflicting_duplicate_aborts_its_durable_cas_session() {
        let directory = TempDir::new().unwrap();
        let card = r#"{"spec":"chara_card_v3","data":{"name":"PNG fixture"}}"#;
        let (source_bytes, _) = png_card_fixture(
            card,
            card,
            &[("duplicate", b"first"), ("duplicate", b"second")],
        );
        let source = directory.path().join("conflicting.png");
        fs::write(&source, source_bytes).unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = start_test_content_job(&state, &source, "conflicting.png");

        let prepared = wait_for_content_job(&state, &started.job_id);

        assert_eq!(prepared.state, JobState::Failed);
        assert_eq!(
            prepared.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid-input")
        );
        assert!(prepared.prepared_content.is_none());
        assert!(crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &started.job_id,
        )
        .is_err());
    }

    #[test]
    fn content_prepare_png_empty_asset_reference_aborts_before_publication() {
        let directory = TempDir::new().unwrap();
        let card = r#"{"spec":"chara_card_v3","data":{"name":"PNG fixture"}}"#;
        let (source_bytes, expected_base) = png_card_fixture(card, card, &[("", b"payload")]);
        let source = directory.path().join("empty-reference.png");
        fs::write(&source, source_bytes).unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = start_test_content_job(&state, &source, "empty-reference.png");

        let prepared = wait_for_content_job(&state, &started.job_id);

        assert_eq!(prepared.state, JobState::Failed);
        assert_eq!(
            prepared.error.as_ref().map(|error| error.code.as_str()),
            Some("invalid-input")
        );
        assert!(prepared.prepared_content.is_none());
        assert!(crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &started.job_id,
        )
        .is_err());
        use sha2::{Digest as _, Sha256};
        let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
        assert!(cas
            .stat_object(&hex::encode(Sha256::digest(expected_base)))
            .unwrap()
            .is_none());
        assert!(cas
            .stat_object(&hex::encode(Sha256::digest(b"payload")))
            .unwrap()
            .is_none());
    }

    #[test]
    fn content_job_claims_one_ready_android_spool_into_its_owned_directory() {
        let directory = TempDir::new().unwrap();
        let job_root = directory.path().join("native-file-jobs");
        let state = NativeFileJobState::initialize(job_root.clone());
        let token = Uuid::new_v4().to_string();
        let source_bytes = br#"{"spec":"chara_card_v3","data":{"name":"Android","assets":[]}}"#;
        write_literal_spool(
            &job_root,
            &token,
            &format!(
                "{{\"token\":\"{token}\",\"state\":\"ready\",\"displayName\":\"android-card.json\",\"bytes\":{},\"totalBytes\":null}}",
                source_bytes.len(),
            ),
            1,
        );
        fs::write(
            directory
                .path()
                .join("native-file-jobs/sources")
                .join(&token)
                .join("source.risudat"),
            source_bytes,
        )
        .unwrap();
        let jobs_root = job_root.join("jobs");
        let unrelated = jobs_root.join("preserve-me");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("sentinel"), b"preserve").unwrap();
        let started = state
            .start_content_for_test(NativeFileJobStartRequest::PrepareContentImport {
                source: JobSource::AndroidSpool {
                    token: token.clone(),
                },
                display_name: "android-card.json".to_owned(),
            })
            .expect("content job should claim the ready Android spool");
        assert!(state
            .start_content_for_test(NativeFileJobStartRequest::PrepareContentImport {
                source: JobSource::AndroidSpool {
                    token: token.clone(),
                },
                display_name: "android-card.json".to_owned(),
            })
            .is_err());

        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            let status = state.status(&started.job_id).unwrap();
            if status.state.is_terminal() {
                break status;
            }
            assert!(Instant::now() < deadline, "content preparation timed out");
            thread::yield_now();
        };
        assert_eq!(status.state, JobState::Succeeded);
        let content = status
            .prepared_content
            .as_ref()
            .expect("prepared Android content");
        assert_eq!(content.cas_session_id, started.job_id);
        assert_eq!(
            content
                .metadata
                .pointer("/data/name")
                .and_then(Value::as_str),
            Some("Android"),
        );
        let session = crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &started.job_id,
        )
        .expect("prepared Android content keeps its durable CAS session");
        assert_eq!(session.pin_count(), 0);
        assert!(!session.is_sealed());
        assert!(!session.is_released());
        assert!(!directory
            .path()
            .join("native-file-jobs/sources")
            .join(token)
            .exists());
        assert!(!jobs_root.join(started.job_id).exists());
        assert_eq!(fs::read(unrelated.join("sentinel")).unwrap(), b"preserve");
    }

    #[test]
    fn content_job_rejects_a_claimed_spool_source_link_without_following_it() {
        let directory = TempDir::new().unwrap();
        let job_root = directory.path().join("native-file-jobs");
        let state = NativeFileJobState::initialize(job_root.clone());
        let token = Uuid::new_v4().to_string();
        let source_bytes = br#"{"spec":"chara_card_v3","data":{"name":"Linked","assets":[]}}"#;
        write_literal_spool(
            &job_root,
            &token,
            &format!(
                "{{\"token\":\"{token}\",\"state\":\"ready\",\"displayName\":\"linked-card.json\",\"bytes\":{},\"totalBytes\":null}}",
                source_bytes.len(),
            ),
            1,
        );
        let spool = job_root.join("sources").join(&token);
        let source = spool.join("source.risudat");
        let linked_payload = spool.join("linked-payload");
        fs::write(&linked_payload, source_bytes).unwrap();
        fs::remove_file(&source).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&linked_payload, &source).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&linked_payload, &source).unwrap();
        let error = state
            .start_content_for_test(NativeFileJobStartRequest::PrepareContentImport {
                source: JobSource::AndroidSpool { token },
                display_name: "linked-card.json".to_owned(),
            })
            .expect_err("content preparation must not follow a claimed spool source link");

        assert_eq!(error.code, "invalid-source");
    }

    #[test]
    fn content_job_uses_the_claimed_spool_display_name_for_classification() {
        let directory = TempDir::new().unwrap();
        let job_root = directory.path().join("native-file-jobs");
        let state = NativeFileJobState::initialize(job_root.clone());
        let token = Uuid::new_v4().to_string();
        let source_bytes = br#"{"spec":"chara_card_v3","data":{"name":"Mismatched","assets":[]}}"#;
        write_literal_spool(
            &job_root,
            &token,
            &format!(
                "{{\"token\":\"{token}\",\"state\":\"ready\",\"displayName\":\"database.risudat\",\"bytes\":{},\"totalBytes\":null}}",
                source_bytes.len(),
            ),
            1,
        );
        fs::write(
            directory
                .path()
                .join("native-file-jobs/sources")
                .join(&token)
                .join("source.risudat"),
            source_bytes,
        )
        .unwrap();
        let error = state
            .start_content_for_test(NativeFileJobStartRequest::PrepareContentImport {
                source: JobSource::AndroidSpool {
                    token: token.clone(),
                },
                display_name: "card.json".to_owned(),
            })
            .expect_err("content preparation must use the claimed spool display name");

        assert_eq!(error.code, "invalid-source");
        assert!(!directory
            .path()
            .join("native-file-jobs/sources")
            .join(token)
            .exists());
    }

    #[test]
    fn content_prepare_appended_charx_jpeg_promotes_the_exact_jpeg_prefix() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("prepared.jpeg");
        let prefix = b"\xff\xd8\xff\xe0RisuNest\xff\xd9";
        fs::write(&source, charx_fixture(Some(prefix))).unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = start_test_content_job(&state, &source, "prepared.jpeg");

        let prepared = wait_for_content_job(&state, &started.job_id);

        assert_eq!(prepared.state, JobState::Succeeded);
        let content = prepared
            .prepared_content
            .as_ref()
            .expect("prepared appended CharX JPEG content");
        assert_eq!(content.cas_session_id, started.job_id);
        let serialized = serde_json::to_value(content).unwrap();
        assert_eq!(
            serialized.pointer("/format").and_then(Value::as_str),
            Some("appended-charx-jpeg")
        );
        let portrait_logical_id = serialized
            .pointer("/portraitLogicalId")
            .and_then(Value::as_str)
            .expect("portrait logical ID");
        assert!(portrait_logical_id.ends_with(".jpg"));
        let portrait = content
            .assets
            .iter()
            .find(|asset| asset.logical_id == portrait_logical_id)
            .expect("portrait logical ID references a prepared asset");
        assert_eq!(portrait.token, "native-appended-portrait");
        assert_eq!(portrait.reference_key, "native-appended-portrait");
        assert_eq!(portrait.ext, "jpg");
        assert_eq!(portrait.mime, "image/jpeg");
        assert_eq!(portrait.byte_size, prefix.len() as u64);
        assert_eq!(prepared.progress.completed_items, 5);
        assert_eq!(prepared.progress.total_items, Some(5));
        let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
        assert_eq!(
            cas.read_object(&portrait.object_hash).unwrap(),
            Some(prefix.to_vec())
        );
        let session = crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &started.job_id,
        )
        .expect("appended CharX keeps its durable CAS session");
        assert_eq!(session.pin_count(), 0);
        assert!(!session.is_sealed());
        assert!(!session.is_released());
    }

    #[test]
    fn content_prepare_ordinary_jpeg_returns_the_stable_destination_error_without_cas_output() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("ordinary.jpeg");
        fs::write(&source, b"\xff\xd8\xff\xd9").unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = start_test_content_job(&state, &source, "ordinary.jpeg");

        let prepared = wait_for_content_job(&state, &started.job_id);

        assert_eq!(prepared.state, JobState::Failed);
        assert_eq!(
            prepared.error.as_ref().map(|error| error.code.as_str()),
            Some("unsupported-without-destination")
        );
        assert!(prepared.prepared_content.is_none());
        assert!(!directory.path().join("assets-v2").exists());
    }

    #[test]
    fn content_parse_failure_aborts_and_releases_its_durable_cas_session() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("invalid-card.json");
        fs::write(
            &source,
            br#"{"spec":"chara_card_v3","data":{"name":"Invalid","assets":[{"type":"icon","uri":"data:image/png;base64,%%%","name":"broken","ext":"png"}]}}"#,
        )
        .unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = start_test_content_job(&state, &source, "invalid-card.json");

        let prepared = wait_for_content_job(&state, &started.job_id);

        assert_eq!(prepared.state, JobState::Failed);
        assert!(prepared.prepared_content.is_none());
        assert!(directory.path().join("assets-v2/job-pins").is_dir());
        assert!(crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &started.job_id,
        )
        .is_err());
    }

    #[test]
    fn content_promotion_failure_aborts_and_releases_its_durable_cas_session() {
        let directory = TempDir::new().unwrap();
        let object_hash = "9f64a747e1b97f131fabb6b447296c9b6f0201e79fb3c5356e6c77e89b6a806a";
        fs::create_dir_all(
            directory
                .path()
                .join("assets-v2/objects")
                .join(&object_hash[..2])
                .join(&object_hash[2..]),
        )
        .unwrap();
        let source = directory.path().join("card.json");
        fs::write(
            &source,
            br#"{"spec":"chara_card_v3","data":{"name":"Promotion failure","assets":[{"type":"icon","uri":"data:image/png;base64,AQIDBA==","name":"main","ext":"png"}]}}"#,
        )
        .unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = start_test_content_job(&state, &source, "card.json");

        let prepared = wait_for_content_job(&state, &started.job_id);

        assert_eq!(prepared.state, JobState::Failed);
        assert!(prepared.prepared_content.is_none());
        assert!(crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &started.job_id,
        )
        .is_err());
    }

    fn prepared_content_with_durable_session(
        repository_root: &Path,
        job: &JobControl,
    ) -> PreparedContent {
        use crate::asset_repository::job_pins::{CasJobKind, DurableCasJob};
        let cas = crate::asset_repository::PayloadCas::new(repository_root).unwrap();
        let session = DurableCasJob::begin(
            repository_root,
            &job.id(),
            CasJobKind::CardOrModuleContentImport,
            1,
        )
        .unwrap();
        cas.prepare_bytes(b"prepared").unwrap();
        assert_eq!(session.pin_count(), 0);
        PreparedContent {
            format: PreparedContentFormat::JsonCard,
            metadata: serde_json::json!({"spec":"chara_card_v3","data":{"name":"Prepared"}}),
            assets: Vec::new(),
            cas_session_id: job.id(),
            portrait_logical_id: None,
            module: None,
        }
    }

    #[test]
    fn accepted_cancel_after_content_prepare_aborts_its_durable_cas_session() {
        let directory = TempDir::new().unwrap();
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::PrepareContentImport).unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        let prepared = prepared_content_with_durable_session(directory.path(), &job);
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::Requested);
        job.finish_content_job_with_cas(Ok(prepared), Ok(()), directory.path())
            .unwrap();

        assert_eq!(job.status().state, JobState::Cancelled);
        assert!(crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &job.id(),
        )
        .is_err());
    }

    #[test]
    fn final_staging_cleanup_failure_aborts_the_prepared_durable_cas_session() {
        let directory = TempDir::new().unwrap();
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::PrepareContentImport).unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        let prepared = prepared_content_with_durable_session(directory.path(), &job);
        job.finish_content_job_with_cas(
            Ok(prepared),
            Err("injected staging cleanup failure".to_owned()),
            directory.path(),
        )
        .unwrap();

        assert_eq!(job.status().state, JobState::Failed);
        assert_eq!(
            job.status().error.as_ref().map(|error| error.code.as_str()),
            Some("cleanup-failed")
        );
        assert!(crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &job.id(),
        )
        .is_err());
    }

    #[test]
    fn content_cancel_during_preparation_finishes_cancelled_and_cleans_staging() {
        let directory = TempDir::new().unwrap();
        let persistent = directory.path().join("persistent");
        fs::create_dir(&persistent).unwrap();
        let active_database = persistent.join("active.sqlite3");
        fs::write(&active_database, b"unchanged-active-database").unwrap();
        let source = directory.path().join("large-card.json");
        let description = "x".repeat(7 * 1024 * 1024);
        fs::write(
            &source,
            serde_json::to_vec(&serde_json::json!({
                "spec": "chara_card_v3",
                "data": {
                    "name": "Cancelled",
                    "description": description,
                    "assets": []
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = state
            .start_content_for_test(NativeFileJobStartRequest::PrepareContentImport {
                source: JobSource::DesktopPath {
                    path: source.to_string_lossy().into_owned(),
                },
                display_name: "large-card.json".to_owned(),
            })
            .expect("start content preparation");

        let deadline = Instant::now() + Duration::from_secs(5);
        while state.status(&started.job_id).unwrap().state == JobState::Queued {
            assert!(
                Instant::now() < deadline,
                "content preparation did not start"
            );
            thread::yield_now();
        }
        assert_eq!(
            state.cancel(&started.job_id).unwrap(),
            CancelOutcome::Requested
        );
        loop {
            let status = state.status(&started.job_id).unwrap();
            if status.state.is_terminal() {
                assert_eq!(status.state, JobState::Cancelled);
                assert!(status.prepared_content.is_none());
                break;
            }
            assert!(Instant::now() < deadline, "content cancellation timed out");
            thread::yield_now();
        }
        assert!(!directory
            .path()
            .join("native-file-jobs/jobs")
            .join(&started.job_id)
            .exists());
        assert!(crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &started.job_id,
        )
        .is_err());
        assert_eq!(
            fs::read(active_database).unwrap(),
            b"unchanged-active-database"
        );
    }

    #[test]
    fn accepted_content_cancel_overrides_preparation_error_after_cleanup() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::PrepareContentImport).unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::Requested);

        job.finish_content_job(
            Err(NativeJobError::new(
                "store-error",
                "progress raced with cancellation",
            )),
            Ok(()),
        )
        .unwrap();

        let status = job.status();
        assert_eq!(status.state, JobState::Cancelled);
        assert!(status.error.is_none());
    }

    #[test]
    fn cancel_request_coordinates_with_finalization_mutex() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::PrepareContentImport).unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        let wait_guard = job.restore_finalized.lock().unwrap();
        let started = Arc::new(Barrier::new(2));
        let (sender, receiver) = std::sync::mpsc::channel();
        let cancelled = Arc::clone(&job);
        let cancel_started = Arc::clone(&started);
        let handle = thread::spawn(move || {
            cancel_started.wait();
            sender.send(cancelled.request_cancel()).unwrap();
        });

        started.wait();
        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
        drop(wait_guard);
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap(),
            CancelOutcome::Requested
        );
        handle.join().unwrap();
    }

    #[test]
    fn export_request_is_an_explicit_path_only_job_contract() {
        let request: NativeFileJobStartRequest = serde_json::from_value(serde_json::json!({
            "kind": "export-block-risu-save",
            "destination": "C:\\chosen\\backup.risudat",
            "expectedRevision": 7,
            "omitAccount": true
        }))
        .unwrap();

        assert!(matches!(
            request,
            NativeFileJobStartRequest::ExportBlockRisuSave {
                destination,
                expected_revision: 7,
                omit_account: true,
            } if destination == "C:\\chosen\\backup.risudat"
        ));
    }

    #[test]
    fn export_job_uses_its_own_ordered_phases() {
        let job = JobRegistry::default()
            .create(JobKind::ExportBlockRisuSave)
            .unwrap();

        job.start(JobPhase::WritingExport).unwrap();
        job.set_phase(JobPhase::PublishingDestination).unwrap();
        job.set_phase(JobPhase::FinalizingExport).unwrap();

        assert_eq!(job.status().phase, JobPhase::FinalizingExport);
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::TooLate);
        assert!(job.set_phase(JobPhase::StagingDatabase).is_err());
    }

    #[test]
    fn running_job_rejects_backward_phases_and_progress_past_known_totals() {
        let job = JobRegistry::default()
            .create(JobKind::RestoreBlockRisuSave)
            .unwrap();
        assert!(job.start(JobPhase::Complete).is_err());
        job.start(JobPhase::ReadingSource).unwrap();
        job.set_phase(JobPhase::StagingDatabase).unwrap();
        assert!(job.set_phase(JobPhase::ReadingSource).is_err());
        assert!(job
            .set_progress(JobProgress {
                completed_bytes: 17,
                total_bytes: Some(16),
                completed_items: 0,
                total_items: None,
            })
            .is_err());
    }

    #[test]
    fn cancel_and_forget_are_idempotent_without_removing_active_jobs() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        job.start(JobPhase::ReadingSource).unwrap();

        assert_eq!(
            registry.cancel(&job.id()).unwrap(),
            CancelOutcome::Requested
        );
        assert_eq!(
            registry.cancel(&job.id()).unwrap(),
            CancelOutcome::AlreadyRequested
        );
        assert!(job.is_cancel_requested());
        assert!(registry.forget(&job.id()).is_err());

        job.finish_cancelled().unwrap();
        assert_eq!(registry.cancel(&job.id()).unwrap(), CancelOutcome::Terminal);
        assert!(registry.forget(&job.id()).unwrap());
        assert!(!registry.forget(&job.id()).unwrap());
    }

    #[test]
    fn terminal_status_rejects_unbounded_results_and_sanitizes_errors() {
        let registry = JobRegistry::default();
        let success = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        success.start(JobPhase::ReadingSource).unwrap();
        assert!(success
            .finish_success(JobResultSummary {
                revision: 2,
                source_bytes: 128,
                source_sha256: "a".repeat(64),
                character_count: 1,
                preset_count: 1,
                warning_codes: (0..=MAX_WARNING_CODES)
                    .map(|index| format!("warning-{index}"))
                    .collect(),
                handoff_path: None,
                recovery_path: None,
            })
            .is_err());

        let failed = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        failed.start(JobPhase::ReadingSource).unwrap();
        failed
            .finish_failure("invalid-source", &("x".repeat(900) + "\nsecret"))
            .unwrap();
        let status = failed.status();
        let error = status.error.unwrap();
        assert_eq!(error.code, "invalid-source");
        assert!(error.message.len() <= MAX_ERROR_MESSAGE_BYTES);
        assert!(!error.message.contains('\n'));
        assert!(status.result.is_none());
    }

    #[test]
    fn terminal_retention_is_bounded_by_count_and_age() {
        let registry = JobRegistry::with_retention(2, Duration::from_secs(60));
        let mut ids = Vec::new();
        for revision in 1..=3 {
            let job = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
            ids.push(job.id());
            job.start(JobPhase::ReadingSource).unwrap();
            job.finish_success(result(revision)).unwrap();
        }

        assert!(registry.status(&ids[0]).is_err());
        assert!(registry.status(&ids[1]).is_ok());
        assert!(registry.status(&ids[2]).is_ok());

        let expiring = JobRegistry::with_retention(2, Duration::ZERO);
        let job = expiring.create(JobKind::RestoreBlockRisuSave).unwrap();
        let id = job.id();
        job.start(JobPhase::ReadingSource).unwrap();
        job.finish_success(result(1)).unwrap();
        assert!(expiring.status(&id).is_err());
    }

    #[test]
    fn prepared_content_success_is_retained_until_explicit_forget() {
        let registry = JobRegistry::with_retention(0, Duration::ZERO);
        let content = registry.create(JobKind::PrepareContentImport).unwrap();
        let content_id = content.id();
        content.start(JobPhase::ReadingSource).unwrap();
        content
            .finish_content_job(
                Ok(PreparedContent {
                    format: PreparedContentFormat::JsonCard,
                    metadata: serde_json::json!({"spec": "chara_card_v3"}),
                    assets: Vec::new(),
                    cas_session_id: content_id.clone(),
                    portrait_logical_id: None,
                    module: None,
                }),
                Ok(()),
            )
            .unwrap();

        let ordinary = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        let ordinary_id = ordinary.id();
        ordinary.start(JobPhase::ReadingSource).unwrap();
        ordinary.finish_success(result(1)).unwrap();

        let cancelled = registry.create(JobKind::PrepareContentImport).unwrap();
        let cancelled_id = cancelled.id();
        cancelled.start(JobPhase::ReadingSource).unwrap();
        assert_eq!(
            cancelled.request_cancel().unwrap(),
            CancelOutcome::Requested
        );
        cancelled
            .finish_content_job(Err(content_cancelled()), Ok(()))
            .unwrap();

        assert!(registry.status(&ordinary_id).is_err());
        assert!(registry.status(&cancelled_id).is_err());
        assert_eq!(
            registry.status(&content_id).unwrap().state,
            JobState::Succeeded
        );
        assert!(registry
            .list()
            .unwrap()
            .iter()
            .any(|status| status.job_id == content_id));
        assert!(registry.forget(&content_id).unwrap());
        assert!(registry.status(&content_id).is_err());
    }

    fn retained_content_fixture(session_id: String) -> PreparedContent {
        PreparedContent {
            format: PreparedContentFormat::JsonCard,
            metadata: serde_json::json!({"spec": "chara_card_v3"}),
            assets: vec![
                PreparedContentAsset {
                    reference_key: "first".to_owned(),
                    token: "first".to_owned(),
                    logical_id: format!("assets/{}.png", "1".repeat(64)),
                    object_hash: "1".repeat(64),
                    byte_size: 4,
                    mime: "image/png".to_owned(),
                    name: "first".to_owned(),
                    ext: "png".to_owned(),
                },
                PreparedContentAsset {
                    reference_key: "second".to_owned(),
                    token: "second".to_owned(),
                    logical_id: format!("assets/{}.json", "2".repeat(64)),
                    object_hash: "2".repeat(64),
                    byte_size: 7,
                    mime: "application/json".to_owned(),
                    name: "second".to_owned(),
                    ext: "json".to_owned(),
                },
            ],
            cas_session_id: session_id,
            portrait_logical_id: None,
            module: None,
        }
    }

    #[test]
    fn succeeded_native_content_receipt_is_the_asset_descriptor_authority() {
        let directory = TempDir::new().unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let job = state
            .registry
            .create(JobKind::PrepareContentImport)
            .unwrap();
        let job_id = job.id();
        job.start(JobPhase::ReadingSource).unwrap();
        job.finish_content_job(Ok(retained_content_fixture(job_id.clone())), Ok(()))
            .unwrap();

        let receipt = state.content_asset_receipt(&job_id).unwrap();

        assert_eq!(receipt, vec![("1".repeat(64), 4), ("2".repeat(64), 7)]);
    }

    #[test]
    fn native_content_receipt_rejects_missing_and_nonterminal_jobs() {
        let directory = TempDir::new().unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        assert!(state.content_asset_receipt("missing-job").is_err());
        let queued = state
            .registry
            .create(JobKind::PrepareContentImport)
            .unwrap();

        assert!(state.content_asset_receipt(&queued.id()).is_err());
    }

    #[test]
    fn native_content_receipt_rejects_a_mismatched_cas_session() {
        let directory = TempDir::new().unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let job = state
            .registry
            .create(JobKind::PrepareContentImport)
            .unwrap();
        let job_id = job.id();
        job.start(JobPhase::ReadingSource).unwrap();
        job.finish_content_job(
            Ok(retained_content_fixture("different-session".to_owned())),
            Ok(()),
        )
        .unwrap();

        assert!(state.content_asset_receipt(&job_id).is_err());
    }

    #[test]
    fn source_descriptors_confine_spool_tokens_and_require_ready_files() {
        let directory = TempDir::new().unwrap();
        let desktop_file = directory.path().join("desktop.risudat");
        fs::write(&desktop_file, b"RISUSAVE\0").unwrap();
        let opened = open_job_source(
            directory.path(),
            &JobSource::DesktopPath {
                path: desktop_file.to_string_lossy().into_owned(),
            },
        )
        .unwrap();
        assert_eq!(opened.total_bytes, 9);
        assert!(open_job_source(
            directory.path(),
            &JobSource::AndroidSpool {
                token: "../outside".to_owned(),
            },
        )
        .is_err());

        let token = Uuid::new_v4().to_string();
        let spool = directory.path().join("sources").join(&token);
        fs::create_dir_all(&spool).unwrap();
        fs::write(spool.join("source.risudat"), b"RISUSAVE\0").unwrap();
        fs::write(
            spool.join("ownership.json"),
            serde_json::to_vec(&SpoolOwnership {
                format: ANDROID_SPOOL_FORMAT.to_owned(),
                version: ANDROID_SPOOL_VERSION,
                token: token.clone(),
                created_at_millis: 1,
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            spool.join("source.json"),
            serde_json::to_vec(&SpoolManifest {
                token: token.clone(),
                state: SpoolState::Copying,
                display_name: "external.risudat".to_owned(),
                bytes: Some(9),
                total_bytes: Some(9),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(open_job_source(
            directory.path(),
            &JobSource::AndroidSpool {
                token: token.clone(),
            },
        )
        .is_err());

        fs::write(
            spool.join("source.json"),
            serde_json::to_vec(&SpoolManifest {
                token: token.clone(),
                state: SpoolState::Ready,
                display_name: "../../external.risudat".to_owned(),
                bytes: Some(9),
                total_bytes: Some(9),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(resolve_source(
            directory.path(),
            &JobSource::AndroidSpool {
                token: token.clone(),
            },
        )
        .is_err());

        fs::write(
            spool.join("source.json"),
            serde_json::to_vec(&SpoolManifest {
                token: token.clone(),
                state: SpoolState::Ready,
                display_name: "external.risudat".to_owned(),
                bytes: Some(9),
                total_bytes: Some(12),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            open_job_source(directory.path(), &JobSource::AndroidSpool { token })
                .unwrap()
                .total_bytes,
            9,
        );
    }

    #[test]
    fn android_spool_rejects_an_owned_directory_escape_before_manifest_access() {
        let directory = TempDir::new().unwrap();
        let sources_root = directory.path().join("sources");
        fs::create_dir_all(&sources_root).unwrap();
        let outside = TempDir::new().unwrap();
        let token = Uuid::new_v4().to_string();
        let spool = sources_root.join(&token);
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(outside.path(), &spool).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &spool).unwrap();

        let error =
            open_job_source(directory.path(), &JobSource::AndroidSpool { token }).unwrap_err();

        assert!(
            error.message.contains("owned directory"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn desktop_source_symlink_is_rejected_without_following() {
        let directory = TempDir::new().unwrap();
        let target = directory.path().join("target.risudat");
        let selected = directory.path().join("selected.risudat");
        fs::write(&target, b"RISUSAVE\0").unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&target, &selected).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &selected).unwrap();

        let error = open_job_source(
            directory.path(),
            &JobSource::DesktopPath {
                path: selected.to_string_lossy().into_owned(),
            },
        )
        .unwrap_err();

        assert_eq!(error.code, "invalid-source");
    }

    #[test]
    fn initialization_records_capability_failure_without_failing_launch() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("native-file-jobs");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("jobs"), b"blocks directory creation").unwrap();

        let state = NativeFileJobState::initialize(root);

        assert_eq!(
            state.capability_error.as_ref().unwrap().code,
            "capability-unavailable"
        );
        assert!(state
            .startup_warnings
            .iter()
            .any(|warning| warning.code == "capability-unavailable"));
        assert!(state.startup_warnings.len() <= MAX_WARNING_CODES);
    }

    #[test]
    fn worker_permits_bound_native_job_threads() {
        let active = Arc::new(AtomicUsize::new(0));
        let first = WorkerPermit::acquire(Arc::clone(&active), 1).unwrap();

        let error = WorkerPermit::acquire(Arc::clone(&active), 1).unwrap_err();

        assert_eq!(error.code, "job-capacity");
        assert_eq!(active.load(Ordering::Acquire), 1);
        drop(first);
        assert!(WorkerPermit::acquire(Arc::clone(&active), 1).is_ok());
    }

    #[test]
    fn startup_cleanup_removes_only_matching_owned_directories() {
        let directory = TempDir::new().unwrap();
        let jobs_root = directory.path().join("jobs");
        fs::create_dir_all(&jobs_root).unwrap();
        let owned_id = Uuid::new_v4().to_string();
        let owned = jobs_root.join(&owned_id);
        fs::create_dir_all(&owned).unwrap();
        fs::write(
            owned.join("ownership.json"),
            serde_json::to_vec(&JobOwnership {
                job_id: owned_id.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(owned.join("partial.tmp"), b"partial").unwrap();

        let unrelated = jobs_root.join("keep-me");
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(unrelated.join("partial.tmp"), b"unrelated").unwrap();
        let mismatched_id = Uuid::new_v4().to_string();
        let mismatched = jobs_root.join(&mismatched_id);
        fs::create_dir_all(&mismatched).unwrap();
        fs::write(
            mismatched.join("ownership.json"),
            serde_json::to_vec(&JobOwnership {
                job_id: Uuid::new_v4().to_string(),
            })
            .unwrap(),
        )
        .unwrap();

        cleanup_owned_directories(&jobs_root).unwrap();

        assert!(!owned.exists());
        assert!(unrelated.exists());
        assert!(mismatched.exists());
        assert!(directory.path().exists());
    }

    #[test]
    fn lossless_handoff_cleanup_removes_only_exact_owned_files_and_is_idempotent() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("native-file-jobs");
        let handoffs = root.join("handoffs");
        fs::create_dir_all(&handoffs).unwrap();
        let owned = handoffs.join(format!("risulossless-{}.risulossless", Uuid::new_v4()));
        let unrelated = handoffs.join("keep.risulossless");
        fs::write(&owned, b"owned").unwrap();
        fs::write(&unrelated, b"unrelated").unwrap();

        assert!(cleanup_lossless_handoff_path(&root, &owned).unwrap());
        assert!(!cleanup_lossless_handoff_path(&root, &owned).unwrap());
        assert!(unrelated.is_file());
        assert_eq!(
            cleanup_lossless_handoff_path(&root, &unrelated)
                .unwrap_err()
                .code,
            "invalid-input"
        );
    }

    #[test]
    fn initialization_preserves_unclaimed_lossless_handoffs_for_reconciliation() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("native-file-jobs");
        let handoffs = root.join("handoffs");
        fs::create_dir_all(&handoffs).unwrap();
        let pending = handoffs.join(format!("risulossless-{}.risulossless", Uuid::new_v4()));
        fs::write(&pending, b"verified export awaiting SAF publication").unwrap();

        let state = NativeFileJobState::initialize(root);

        assert!(pending.is_file());
        assert!(state.startup_warnings.is_empty());
    }

    #[test]
    fn startup_cleanup_removes_only_manifest_owned_spool_directories() {
        let directory = TempDir::new().unwrap();
        let sources_root = directory.path().join("sources");
        fs::create_dir_all(&sources_root).unwrap();
        let token = Uuid::new_v4().to_string();
        let owned = sources_root.join(&token);
        fs::create_dir_all(&owned).unwrap();
        fs::write(owned.join("source.risudat"), b"partial").unwrap();
        fs::write(
            owned.join("ownership.json"),
            serde_json::to_vec(&SpoolOwnership {
                format: ANDROID_SPOOL_FORMAT.to_owned(),
                version: ANDROID_SPOOL_VERSION,
                token: token.clone(),
                created_at_millis: 0,
            })
            .unwrap(),
        )
        .unwrap();
        fs::write(
            owned.join("source.json"),
            serde_json::to_vec(&SpoolManifest {
                token: token.clone(),
                state: SpoolState::Copying,
                display_name: "chosen.risudat".to_owned(),
                bytes: None,
                total_bytes: None,
            })
            .unwrap(),
        )
        .unwrap();
        let unrelated = sources_root.join("not-a-token");
        fs::create_dir_all(&unrelated).unwrap();

        cleanup_spool_directories(&sources_root).unwrap();

        assert!(!owned.exists());
        assert!(unrelated.exists());
    }
}
