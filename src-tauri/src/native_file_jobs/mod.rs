mod portrait_rendition;
pub(crate) mod admission;
mod character_charx_export;
mod character_json_export;
mod character_png_export;
pub mod charx;
mod content;
mod available_payload;
mod dataset_export;
mod gallery_export;
pub(crate) mod download_handoff;
mod error;
mod jpeg_asset;
pub mod screenshot_output;

mod backup_source;
mod legacy_backup;
mod restore_receipts;
mod portable_export_receipts;
pub(crate) mod snapshot_bodies;
mod snapshot_restore;
#[cfg(test)]
pub(crate) mod portable;
#[cfg(not(test))]
mod portable;
pub(crate) mod portable_source_custody;

#[tauri::command(async)]
pub(crate) fn native_portable_source_discard(app:AppHandle,source:JobSource)->Result<bool,NativeJobError> {
    logged("native_portable_source_discard", (|| {
        portable_source_custody::ensure_platform_source(&app,&source)?;
        portable_source_custody::discard(&source)
    })())
}

#[tauri::command(async)]
pub(crate) fn native_portable_source_cleanup_orphans(app:AppHandle)->Result<portable_source_custody::InterruptedSources,NativeJobError> {
    logged("native_portable_source_cleanup_orphans", portable_source_custody::cleanup_orphans(&app))
}
pub(crate) mod raw_recovery;
pub(crate) use backup_source::*;
mod official_snapshot;
pub(crate) mod official_assets;
mod transfer_retry;
mod risum_export;
mod verified_read;
#[cfg(windows)]
mod windows_cloud_source;

#[cfg(test)]
mod screenshot_output_test;

use crate::native_log::logged;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
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
const HANDOFF_STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
const ANDROID_SPOOL_STAGING_PREFIX: &str = ".spooling-";
const ANDROID_SPOOL_CLEANUP_PREFIX: &str = ".cleanup-";
// Keep Rust ownership changes exclusive, then retain the opened file so cleanup
// initiated outside Rust cannot invalidate an already successful claim on POSIX.
static ANDROID_SPOOL_OWNERSHIP_GATE: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeJobError {
    pub(crate) code: String,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) item_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) owner: Option<FailureOwner>,
}

/// The library item whose data failed. Its name stays out of the message,
/// which is logged.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FailureOwner {
    pub(crate) kind: &'static str,
    pub(crate) name: String,
    /// The character a chat belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) character: Option<String>,
}

impl NativeJobError {
    pub(crate) fn new(code: &str, message: impl AsRef<str>) -> Self {
        Self {
            code: code.to_owned(),
            message: bounded_message(message.as_ref()),
            item_kind: None,
            owner: None,
        }
    }

    pub(crate) fn too_large(item_kind: &str, message: impl AsRef<str>) -> Self {
        Self { item_kind: Some(item_kind.to_owned()), ..Self::new("payload-too-large", message) }
    }

    /// Names the owner unless a nearer one is already named.
    pub(crate) fn owned_by(mut self, owner: &FailureOwner) -> Self {
        self.owner.get_or_insert_with(|| owner.clone());
        self
    }
}

impl std::fmt::Display for NativeJobError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for NativeJobError {}

impl crate::native_log::CommandFailure for NativeJobError {
    fn code(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.code)
    }

    fn detail(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some(std::borrow::Cow::Borrowed(&self.message))
    }

    fn expected(&self) -> bool {
        matches!(
            self.code.as_str(),
            "cancelled" | "library-operation-busy" | "job-capacity" | "cleanup-pending" | "revision-conflict"
        )
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum JobSource {
    DesktopPath { path: String },
    AndroidSpool { token: String },
    AndroidSeekable { token: String },
    IosScoped { token: String },
    SavedConflict { token: String },
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
    #[serde(deserialize_with = "deserialize_required_nullable_u64")]
    total_bytes: Option<u64>,
    import_destination: Option<String>,
    operation_id: Option<String>,
}

fn deserialize_required_nullable_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<u64>::deserialize(deserializer)
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

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub(crate) enum OfficialPublicationCredential {
    RisuAuth { token: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OfficialPublicationRetryRequest {
    pub(crate) job_id: String,
    pub(crate) account_id: String,
    pub(crate) session: Option<String>,
    pub(crate) save_date: String,
    pub(crate) credential: OfficialPublicationCredential,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(
    tag = "kind",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum OfficialPublicationAttemptResult {
    Written {
        account_id: String,
        session: Option<String>,
        save_date: String,
        status: u16,
        replacement_key: String,
        warning: Option<String>,
        reload_session: bool,
    },
    NotModified {
        account_id: String,
        session: Option<String>,
        save_date: String,
        status: u16,
        replacement_key: String,
    },
    AuthWarning {
        account_id: String,
        session: Option<String>,
        save_date: String,
        status: u16,
        warning: Option<String>,
    },
    ReauthenticationNeeded {
        account_id: String,
        session: Option<String>,
        save_date: String,
        status: u16,
        warning: Option<String>,
    },
}

#[cfg(feature = "native-official-publication")]
#[derive(Clone)]
pub(crate) struct OfficialPublicationJobRequest {
    pub(crate) lease: String,
    pub(crate) expected_revision: i64,
    pub(crate) account_id: String,
    pub(crate) base_url: String,
    pub(crate) replacements: HashMap<String, String>,
    pub(crate) session: Option<String>,
    pub(crate) save_date: String,
    pub(crate) credential: OfficialPublicationCredential,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OfficialPublicationRetryInput {
    pub(crate) session: Option<String>,
    pub(crate) save_date: String,
    pub(crate) credential: OfficialPublicationCredential,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CharacterCharxContainer {
    #[default]
    PlainCharx,
    AppendedCharxJpeg,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum CharacterCardExportFormat {
    JsonCard,
    PngCard,
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
    RestoreOfficialAccountSnapshot {
        base_url: String,
        credential: official_snapshot::OfficialSnapshotCredential,
        expected_revision: i64,
    },
    RestoreLegacyLocalBackup {
        source: JobSource,
        expected_revision: i64,
    },
    ExportBlockRisuSave {
        destination: String,
        expected_revision: i64,
        #[serde(default)]
        omit_account: bool,
    },
    ExportPortableBackup {
        destination: Option<String>,
        expected_revision: Option<i64>,
        source: Option<JobSource>,
        selection: portable::PortableSelection,
    },
    ExportRawRecovery {
        destination: Option<String>,
    },
    RestorePortableBackup {
        source: JobSource,
        expected_revision: i64,
        selection: Option<portable::PortableSelection>,
    },
    RestoreNativeSnapshot {
        snapshot_id: String,
        expected_revision: i64,
    },
    ExportCompatibleLocalBackup {
        target: legacy_backup::CompatibilityTarget,
        destination: Option<String>,
        expected_revision: i64,
    },
    ExportCharacterCharx {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        portrait_rendition: Option<portrait_rendition::PortraitRenditionDescriptor>,
        destination: Option<String>,
        expected_revision: i64,
        character_id: String,
        #[serde(default)]
        container: CharacterCharxContainer,
        #[serde(default)]
        card: Option<Value>,
        #[serde(default)]
        module: Option<Value>,
    },
    ExportCharacterCard {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        portrait_rendition: Option<portrait_rendition::PortraitRenditionDescriptor>,
        destination: Option<String>,
        expected_revision: i64,
        character_id: String,
        format: CharacterCardExportFormat,
        metadata: Value,
    },
    ExportRisuModule {
        destination: Option<String>,
        expected_revision: i64,
        module_index: u64,
    },
    ExportDataset {
        destination: Option<String>,
        expected_revision: i64,
    },
    ExportGalleryFile {
        destination: Option<String>,
        expected_revision: i64,
        selection: gallery_export::GallerySelection,
    },
    ExportGalleryZip {
        destination: Option<String>,
        expected_revision: i64,
        selection: gallery_export::GallerySelection,
    },
    PrepareContentImport {
        source: JobSource,
        display_name: String,
    },
    ImportJpegAsset {
        source: JobSource,
        display_name: String,
        destination: jpeg_asset::JpegAssetDestination,
        expected_revision: i64,
    },
    KeiBackupUpload {
        lease: String,
        expected_revision: i64,
        url: String,
        expected_account_id: String,
        token: String,
    },
    OfficialPublicationUpload {
        lease: String,
        expected_revision: i64,
        account_id: String,
        base_url: String,
        replacements: HashMap<String, String>,
        session: Option<String>,
        save_date: String,
        credential: OfficialPublicationCredential,
    },
    RestoreOfficialAccountAssets {
        base_url: String,
        credential: official_snapshot::OfficialSnapshotCredential,
        account_id: String,
        expected_library: String,
        key_count: u64,
        #[serde(default)]
        concurrency: Option<u8>,
        reauthenticated: bool,
    },
    PublishOfficialAccountAssets {
        base_url: String,
        credential: official_snapshot::OfficialSnapshotCredential,
        account_id: String,
        lease: String,
        #[serde(default)]
        session: Option<String>,
        assets: Vec<official_assets::upload::PinnedAsset>,
        #[serde(default)]
        concurrency: Option<u8>,
    },
}

fn portable_export_requires_revision(source: &Option<JobSource>, expected_revision: Option<i64>, _selection: &portable::PortableSelection) -> Result<bool, NativeJobError> {
    if source.is_none() && expected_revision.is_some() { Ok(false) }
    else { Err(NativeJobError::new("invalid-input", "Portable export requires an active revision")) }
}


#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NativeFileJobStarted {
    pub(crate) job_id: String,
    pub(crate) warning_codes: Vec<String>,
}

#[derive(Clone,Debug,Serialize,Deserialize)]
#[serde(rename_all="camelCase")]
pub(crate) struct NativeSnapshotBodiesStarted {
    pub(crate) job_id:String,
    pub(crate) kind:JobKind,
    pub(crate) staging_id:String,
    pub(crate) activation_revision:String,
    pub(crate) binding_authority:String,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PreparedContentFormat {
    JsonCard,
    PngCard,
    CharxCard,
    AppendedCharxJpeg,
    RisuModule,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreparedContentAsset {
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) reference_key: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(crate) token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) position: Option<usize>,
    pub(crate) logical_id: String,
    pub(crate) object_hash: String,
    pub(crate) byte_size: u64,
    pub(crate) mime: String,
    pub(crate) name: String,
    pub(crate) ext: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreparedContentOwnerHead {
    pub(crate) present: bool,
    pub(crate) manifest_hash: Option<String>,
    pub(crate) entry_count: usize,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) owner_head: Option<PreparedContentOwnerHead>,
}

#[derive(Debug)]
pub(crate) struct OpenedJobSource {
    pub(crate) file: File,
    pub(crate) total_bytes: u64,
    pub(crate) custody: Option<portable_source_custody::PortableSourceCustodyLease>,
}

#[derive(Debug)]
struct ClaimedSpoolSource {
    #[cfg(test)]
    path: PathBuf,
    opened: OpenedJobSource,
}

fn open_regular_file_no_follow(path: &Path) -> Result<OpenedJobSource, NativeJobError> {
    open_source_file(path, false)
}

fn open_source_file(path: &Path, _allow_cloud_source: bool) -> Result<OpenedJobSource, NativeJobError> {
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
    #[cfg(windows)]
    if is_reparse_point && _allow_cloud_source && metadata.is_file() && !metadata.file_type().is_symlink() {
        let file = windows_cloud_source::reopen_cloud_source(file)?;
        let metadata = file.metadata().map_err(|error| invalid_source_error(error.to_string()))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(invalid_source_error("source must be a regular file"));
        }
        return Ok(OpenedJobSource { file, total_bytes: metadata.len(), custody: None });
    }
    if metadata.file_type().is_symlink() || is_reparse_point || !metadata.is_file() {
        return Err(invalid_source_error("source must be a regular file"));
    }
    Ok(OpenedJobSource {
        file,
        total_bytes: metadata.len(),
        custody: None,
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
            open_source_file(path, true)
        }
        JobSource::AndroidSpool { token } => {
            let path = resolve_spool_source(job_root, token)?;
            open_regular_file_no_follow(&path)
        }
        JobSource::SavedConflict { .. } | JobSource::AndroidSeekable { .. } | JobSource::IosScoped { .. } => Err(invalid_source_error("Portable custody requires its bounded probe or exact job claim")),
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
        JobSource::SavedConflict { .. } | JobSource::AndroidSeekable { .. } | JobSource::IosScoped { .. } => Err(invalid_source_error("Portable custody has no path authority")),
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
) -> Result<ClaimedSpoolSource, NativeJobError> {
    claim_spool_source_with_display_name(job_root, token, owned_directory, None)
}

fn claim_spool_content_source(
    job_root: &Path,
    token: &str,
    owned_directory: &Path,
    display_name: &str,
) -> Result<ClaimedSpoolSource, NativeJobError> {
    claim_spool_source_with_display_name(job_root, token, owned_directory, Some(display_name))
}

fn claim_spool_source_with_display_name(
    job_root: &Path,
    token: &str,
    owned_directory: &Path,
    expected_display_name: Option<&str>,
) -> Result<ClaimedSpoolSource, NativeJobError> {
    let _ownership = ANDROID_SPOOL_OWNERSHIP_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    let path = validate_spool_source(&canonical_claimed, token, expected_display_name)?;
    let opened = open_regular_file_no_follow(&path)?;
    Ok(ClaimedSpoolSource {
        #[cfg(test)]
        path,
        opened,
    })
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
        || manifest.operation_id.as_ref().is_some_and(|id| {
            id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
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
        && name != "."
        && name != ".."
        && name.chars().count() <= 180
        && name
            .chars()
            .all(|character| !character.is_control()
                && !matches!(character, '/' | '\\' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
}

#[cfg(test)]
#[test]
fn spool_display_names_preserve_unicode_without_path_or_display_controls() {
    for name in ["캐릭터 이름.json", "設定.risum", "人物😀.png", "e\u{301}.json", &format!("{}.json", "😀".repeat(175))] {
        assert!(is_safe_spool_display_name(name), "{name:?}");
    }
    for name in ["", ".", "..", "../card.png", "a\\b.json", "a\u{85}.json", "a\u{202e}.json", "a\u{2066}.png", &"😀".repeat(181)] {
        assert!(!is_safe_spool_display_name(name), "{name:?}");
    }
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

fn handoff_name(path: &Path, prefix: &str, suffix: &str) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(token) = name
        .strip_prefix(prefix)
        .and_then(|name| name.strip_suffix(suffix))
    else {
        return false;
    };
    Uuid::parse_str(token).is_ok_and(|id| {
        id.hyphenated().to_string() == token
            && id.get_version() == Some(uuid::Version::Random)
            && id.get_variant() == uuid::Variant::RFC4122
    })
}

/// Checks that `path` is an app-owned handoff file; `false` when it is gone.
fn owned_handoff_file(
    root: &Path,
    path: &Path,
    prefix: &str,
    suffix: &str,
    label: &str,
    failure_code: &str,
) -> Result<bool, NativeJobError> {
    let handoffs_root = root.join("handoffs").canonicalize().map_err(|error| {
        NativeJobError::new(
            failure_code,
            format!("{label} handoff root cannot be resolved: {error}"),
        )
    })?;
    if !path.is_absolute()
        || path.parent().and_then(|parent| parent.canonicalize().ok())
            != Some(handoffs_root.clone())
        || !handoff_name(path, prefix, suffix)
    {
        return Err(NativeJobError::new(
            "invalid-input",
            format!("{label} handoff target is not app-owned"),
        ));
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(NativeJobError::new(
                failure_code,
                format!("{label} handoff metadata is unavailable: {error}"),
            ))
        }
    };
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(NativeJobError::new(
            "invalid-input",
            format!("{label} handoff target is not an owned regular file"),
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(NativeJobError::new(
                "invalid-input",
                format!("{label} handoff target is a reparse point"),
            ));
        }
    }
    Ok(true)
}

fn cleanup_handoff_path(
    root: &Path,
    path: &Path,
    prefix: &str,
    suffix: &str,
    label: &str,
) -> Result<bool, NativeJobError> {
    if !owned_handoff_file(root, path, prefix, suffix, label, "cleanup-failed")? {
        return Ok(false);
    }
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(NativeJobError::new(
            "cleanup-failed",
            format!("{label} handoff cannot be removed: {error}"),
        )),
    }
}

fn cleanup_legacy_backup_handoff_path(root: &Path, path: &Path) -> Result<bool, NativeJobError> {
    cleanup_handoff_path(root, path, "risu-backup-", ".bin", "legacy backup")
}

fn is_owned_handoff_name(path: &Path) -> bool {
    handoff_name(path, "risunest-backup-", ".risunest")
        || handoff_name(path, "risunest-rescue-", ".risunest-rescue.zip")
        || handoff_name(path, "risu-backup-", ".bin")
        || handoff_name(path, "risu-charx-", ".charx")
        || handoff_name(path, "risu-charx-", ".jpeg")
        || handoff_name(path, "risu-character-card-", ".json")
        || handoff_name(path, "risu-character-card-", ".png")
        || handoff_name(path, "risu-module-", ".risum")
        || handoff_name(path, dataset_export::PREFIX, dataset_export::SUFFIX)
        || handoff_name(path, download_handoff::PREFIX, download_handoff::SUFFIX)
}

fn cleanup_stale_handoffs(root: &Path) -> Result<(), String> {
    cleanup_stale_handoffs_at(root, SystemTime::now(), HANDOFF_STALE_AFTER)
}

fn cleanup_stale_handoffs_at(
    handoffs_root: &Path,
    now: SystemTime,
    stale_after: Duration,
) -> Result<(), String> {
    if !handoffs_root.is_dir() {
        return Ok(());
    }
    let canonical_root = handoffs_root
        .canonicalize()
        .map_err(|error| format!("native handoff root cannot be resolved: {error}"))?;
    let retained = portable_export_receipts::retained_handoffs(handoffs_root.parent().unwrap_or(handoffs_root));
    let mut errors = Vec::new();
    for entry in fs::read_dir(&canonical_root)
        .map_err(|error| format!("native handoff root cannot be read: {error}"))?
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                record_cleanup_error(
                    &mut errors,
                    format!("native handoff entry cannot be read: {error}"),
                );
                continue;
            }
        };
        let path = entry.path();
        if !is_owned_handoff_name(&path) || retained.contains(&path) {
            continue;
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                record_cleanup_error(
                    &mut errors,
                    format!("native handoff metadata is unavailable: {error}"),
                );
                continue;
            }
        };
        if !metadata.is_file()
            || crate::trust_boundary::is_link_like(&metadata)
            || metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .is_none_or(|age| age < stale_after)
        {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => record_cleanup_error(
                &mut errors,
                format!("native handoff cannot be removed: {error}"),
            ),
        }
    }
    cleanup_errors_result(errors)
}

fn cleanup_character_charx_handoff_path(root: &Path, path: &Path) -> Result<bool, NativeJobError> {
    let suffix = match path.extension().and_then(|extension| extension.to_str()) {
        Some("charx") => ".charx",
        Some("jpeg") => ".jpeg",
        _ => {
            return Err(NativeJobError::new(
                "invalid-input",
                "character CharX handoff cleanup target is not app-owned",
            ))
        }
    };
    cleanup_handoff_path(root, path, "risu-charx-", suffix, "character CharX")
}

fn cleanup_character_card_handoff_path(root: &Path, path: &Path) -> Result<bool, NativeJobError> {
    let suffix = match path.extension().and_then(|extension| extension.to_str()) {
        Some("json") => ".json",
        Some("png") => ".png",
        _ => {
            return Err(NativeJobError::new(
                "invalid-input",
                "character card handoff cleanup target is not app-owned",
            ))
        }
    };
    cleanup_handoff_path(root, path, "risu-character-card-", suffix, "character card")
}

fn cleanup_risu_module_handoff_path(root: &Path, path: &Path) -> Result<bool, NativeJobError> {
    cleanup_handoff_path(root, path, "risu-module-", ".risum", "RISUM")
}

fn cleanup_dataset_handoff_path(root: &Path, path: &Path) -> Result<bool, NativeJobError> {
    cleanup_handoff_path(root, path, dataset_export::PREFIX, dataset_export::SUFFIX, "dataset")
}

fn cleanup_spool_directories_at(
    sources_root: &Path,
    now_millis: u64,
    stale_after_millis: u64,
) -> Result<(), String> {
    let _ownership = ANDROID_SPOOL_OWNERSHIP_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    // Windows answers a removal of a directory another thread is already
    // deleting with a permission error, and the name survives until that
    // deletion completes. Yielding alone loses the race whenever the winner is
    // descheduled, so the wait falls back to short sleeps before giving up.
    for attempt in 0..128 {
        match fs::symlink_metadata(path) {
            Err(probe) if probe.kind() == std::io::ErrorKind::NotFound => return true,
            _ if attempt < 16 => std::thread::yield_now(),
            _ => std::thread::sleep(std::time::Duration::from_millis(1)),
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

#[derive(Clone)]
pub(crate) struct NativeFileJobState {
    snapshot_body_start: Arc<Mutex<()>>,
    cleanup_closed: Arc<std::sync::RwLock<bool>>,
    root: PathBuf,
    registry: Arc<JobRegistry>,
    pub(crate) admission: Arc<admission::Admission>,
    active_workers: Arc<AtomicUsize>,
    max_concurrent_jobs: usize,
    startup_warnings: Vec<NativeJobError>,
    capability_error: Arc<Mutex<Option<NativeJobError>>>,
}

impl NativeFileJobState {
    pub(crate) fn prepare_snapshot_bodies(&self,store:&mut crate::persistent_store::PersistentStore,stage:&str,revision:i64,authority:&str)->Result<(crate::persistent_store::PersistentStore,snapshot_bodies::BodyPlan,Arc<JobControl>),NativeJobError> {
        let plan=store.snapshot_restore_body_plan(stage,revision,authority).map_err(error::store_error)?;
        let worker_store=store.open_native_job_store().map_err(error::store_error)?;
        let job=self.registry.create_internal(JobKind::SnapshotBodies,Some(revision),Vec::new(),false).map_err(|error|NativeJobError::new("store-error",error))?;
        {
            let mut status=job.status.lock().map_err(|_|NativeJobError::new("store-error","Snapshot job status is unavailable"))?;
            status.snapshot_staging_id=Some(stage.to_owned());
            status.activation_revision=Some(revision);
            status.activation_authority=Some(authority.to_owned());
        }
        Ok((worker_store,plan,job))
    }
    #[cfg(test)]
    pub(crate) fn create_portable_restore_fixture(&self,revision:i64)->Result<(Arc<JobControl>,Arc<Mutex<Option<admission::Permit>>>),NativeJobError> {
        let permit=self.admission.file(true).map_err(|error|NativeJobError::new("library-operation-busy",error))?;
        let slot=Arc::new(Mutex::new(Some(permit)));
        let job=self.registry.create_with_context(JobKind::RestorePortableBackup,Some(revision),Vec::new()).map_err(|error|NativeJobError::new("store-error",error))?;
        job.set_activation_permit(Arc::downgrade(&slot)).map_err(|error|NativeJobError::new("store-error",error))?;
        Ok((job,slot))
    }

    #[cfg(test)]
    pub(crate) fn select_portable_restore_fixture(&self,id:&str,selection:portable::PortableSelection)->Result<(),NativeJobError> {
        let job=self.registry.lookup(id).map_err(|error|NativeJobError::new("store-error",error))?.ok_or_else(||NativeJobError::new("job-not-found","Portable restore owner is unavailable"))?;
        job.select_portable_sections(selection).map_err(|error|NativeJobError::new("invalid-selection",error))
    }

    pub(crate) fn confirm_portable_restore_adoption(&self,device:&crate::device_backup::DeviceBackupState,id:&str,revision:&str,authority:&str,session:&str)->Result<(),NativeJobError> {
        let job=self.registry.lookup(id).map_err(|error|NativeJobError::new("store-error",error))?.ok_or_else(||NativeJobError::new("job-not-found","Portable restore owner is unavailable"))?;
        job.confirm_portable_adoption(revision,authority,session,||device.verify_portable_adoption_complete(session,id,revision,authority).map_err(|error|error.message))
            .map_err(|error|NativeJobError::new("invalid-activation-receipt",error))
    }
    pub(crate) fn retry_portable_restore_bodies(&self,request:&PortableBodyRetryRequest)->Result<NativeFileJobStarted,NativeJobError> {
        let _cleanup=self.admit_cleanup_operation()?;
        let repository=self.root.parent().ok_or_else(||NativeJobError::new("store-error","Native repository is unavailable"))?;
        verify_portable_body_receipt(repository,request)?;
        let job=self.registry.lookup(&request.job_id).map_err(|error|NativeJobError::new("store-error",error))?
            .ok_or_else(||NativeJobError::new("portable-body-source-required","Portable library is restored, but the backup source is required"))?;
        let status=job.status();
        if status.kind!=JobKind::RestorePortableBackup || !status.restore_adoption_confirmed
            || status.activation_revision.map(|revision|revision.to_string()).as_deref()!=Some(request.activation_revision.as_str())
            || status.activation_authority.as_deref()!=Some(request.binding_authority.as_str())
            || status.device_session_id.as_deref()!=Some(request.device_session_id.as_str())
            || !status.portable_body_retry.as_ref().is_some_and(|receipt|receipt.staging_id==request.staging_id && receipt.catalog_sha256==request.catalog_sha256)
        {return Err(NativeJobError::new("invalid-activation-receipt","Portable body retry receipt differs"));}
        let receipt=status.portable_body_retry.as_ref().expect("validated receipt");
        if receipt.source_required {return Err(NativeJobError::new("portable-body-source-required","Portable library is restored, but the backup source is required"));}
        if receipt.available {job.retry_portable_bodies(&status).map_err(|error|NativeJobError::new(if error=="portable-body-source-required" {"portable-body-source-required"} else {"portable-body-retry-refused"},error))?;}
        Ok(NativeFileJobStarted {job_id:request.job_id.clone(),warning_codes:status.warning_codes})
    }

    pub(crate) fn admit_cleanup_operation(&self) -> Result<std::sync::RwLockReadGuard<'_, bool>, NativeJobError> {
        let guard = self.cleanup_closed.read().map_err(|_| NativeJobError::new("store-error", "Native cleanup state is unavailable"))?;
        if *guard { return Err(NativeJobError::new("cleanup-pending", "Application cleanup is pending")); }
        Ok(guard)
    }

    pub(crate) fn finish_cleanup(&self) -> Result<(), String> {
        *self.cleanup_closed.write().map_err(|_| "cleanup-native-jobs-busy")? = false;
        Ok(())
    }

    pub(crate) fn begin_cleanup(&self) -> Result<(), String> {
        *self.cleanup_closed.try_write().map_err(|_| "cleanup-native-jobs-busy")? = true;
        for job in self.registry.list()? {
            if job.kind==JobKind::RestorePortableBackup {
                if let Some(owner)=self.registry.lookup(&job.job_id)? {owner.retire_portable_body_owner()?;}
            }
            self.registry.cancel(&job.job_id)?;
        }
        Ok(())
    }

    pub(crate) fn cleanup_drained(&self) -> bool {
        self.active_workers.load(Ordering::Acquire) == 0
    }

    pub(crate) fn close_for_cleanup(&self) -> Result<(), String> {
        if self.active_workers.load(Ordering::Acquire) != 0 { return Err("cleanup-native-jobs-busy".into()); }
        self.registry.jobs.lock().map_err(|_| "cleanup-native-jobs-busy")?.clear();
        Ok(())
    }

    pub(crate) fn reopen_after_cleanup(&self) -> Result<(), String> {
        for directory in ["jobs", "sources", "handoffs"] {
            fs::create_dir_all(self.root.join(directory)).map_err(|_| "cleanup-native-jobs-unavailable")?;
        }
        self.capability_error.lock().map_err(|_| "cleanup-native-jobs-unavailable")?.take();
        Ok(())
    }

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
            (&handoffs_root, "native backup handoff"),
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
        let registry = Arc::new(JobRegistry::default());
        if let Err(error) = portable_export_receipts::restore(&root, &registry) {
            startup_warnings.push(NativeJobError::new("export-receipt-unavailable", error));
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
        if handoffs_root.is_dir() {
            if let Err(error) = cleanup_stale_handoffs(&handoffs_root) {
                startup_warnings.push(NativeJobError::new("cleanup-failed", error));
            }
        }
        startup_warnings.truncate(MAX_WARNING_CODES);
        Self {
            snapshot_body_start: Arc::new(Mutex::new(())),
            cleanup_closed: Arc::new(std::sync::RwLock::new(false)),
            root,
            registry,
            admission: Arc::new(admission::Admission::default()),
            active_workers: Arc::new(AtomicUsize::new(0)),
            max_concurrent_jobs,
            startup_warnings,
            capability_error: Arc::new(Mutex::new(capability_error)),
        }
    }

    pub(crate) fn start(
        &self,
        request: NativeFileJobStartRequest,
        app: AppHandle,
    ) -> Result<NativeFileJobStarted, NativeJobError> {
        let _cleanup_operation = self.admit_cleanup_operation()?;
        if let Some(error) = self.capability_error.lock().map_err(|_| NativeJobError::new("store-error", "Native capability state is unavailable"))?.as_ref() {
            return Err(error.clone());
        }
        let task = match request {
            NativeFileJobStartRequest::RestoreBlockRisuSave {
                source,
                expected_revision,
            } => {
                let opened_source = match &source {
                    JobSource::SavedConflict { .. } => None,
                    JobSource::DesktopPath { .. } => Some(open_job_source(&self.root, &source)?),
                    JobSource::AndroidSpool { token } => {
                        parse_android_spool_token(token)?;
                        None
                    }
                    JobSource::AndroidSeekable { .. } | JobSource::IosScoped { .. } => return Err(invalid_source_error("Portable custody is not an upstream/content source")),
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
            NativeFileJobStartRequest::RestoreOfficialAccountSnapshot {
                base_url,
                credential,
                expected_revision,
            } => NativeFileJobTask::RestoreOfficialSnapshot {
                request: official_snapshot::OfficialSnapshotRestoreRequest {
                    base_url,
                    credential,
                },
                expected_revision,
                app,
            },
            NativeFileJobStartRequest::RestoreLegacyLocalBackup {
                source,
                expected_revision,
            } => {
                let opened_source = match &source {
                    JobSource::DesktopPath { .. } => Some(open_job_source(&self.root, &source)?),
                    JobSource::AndroidSpool { token } => {
                        parse_android_spool_token(token)?;
                        None
                    }
                    JobSource::SavedConflict { .. } | JobSource::AndroidSeekable { .. } | JobSource::IosScoped { .. } => return Err(invalid_source_error("Portable custody is not an upstream/content source")),
                };
                let repository_root =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        Ok(store.repository_root().to_path_buf())
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::RestoreLegacyLocalBackup {
                    opened_source,
                    source,
                    expected_revision,
                    repository_root,
                    app,
                }
            }
            NativeFileJobStartRequest::ExportPortableBackup {
                destination,
                expected_revision,
                source,
                selection,
            } => {
                let destination = destination.map(PathBuf::from);
                if let Some(path) = destination.as_deref() {
                    validate_desktop_destination(path)?;
                }
                let store = if portable_export_requires_revision(
                    &source,
                    expected_revision,
                    &selection,
                )? {
                    None
                } else {
                    Some(
                        crate::persistent_store::commands::with_store_mut(
                            app.state(),
                            |store| store.open_native_job_store(),
                        )
                        .map_err(native_store_error)?,
                    )
                };
                NativeFileJobTask::ExportPortable {
                    destination,
                    expected_revision,
                    store,
                    source,
                    selection,
                    app,
                }
            }
            NativeFileJobStartRequest::ExportRawRecovery { destination } => {
                let destination = destination.map(PathBuf::from);
                if let Some(path) = destination.as_deref() {
                    validate_desktop_destination(path)?;
                }
                let data_root = self.root.parent().ok_or_else(|| {
                    NativeJobError::new(
                        "capability-unavailable",
                        "Native application data root is unavailable",
                    )
                })?.to_path_buf();
                NativeFileJobTask::ExportRawRecovery {
                    destination,
                    data_root,
                    app_version: app.package_info().version.to_string(),
                    app,
                    capture_guard: None,
                }
            }
            NativeFileJobStartRequest::RestorePortableBackup {
                source,
                expected_revision,
                selection,
            } => {
                let opened_source = match &source {
                    JobSource::DesktopPath { .. } => Some(open_job_source(&self.root, &source)?),
                    JobSource::AndroidSpool { token } => {
                        parse_android_spool_token(token)?;
                        None
                    }
                    JobSource::SavedConflict { .. } | JobSource::AndroidSeekable { .. } | JobSource::IosScoped { .. } => { portable_source_custody::ensure_platform_source(&app, &source)?; None },
                };
                let store =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        store.open_native_job_store()
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::RestorePortable {
                    opened_source,
                    source,
                    expected_revision,
                    store,
                    selection,
                    app,
                }
            }
            NativeFileJobStartRequest::RestoreNativeSnapshot {
                snapshot_id,
                expected_revision,
            } => {
                let store =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        store.open_native_job_store()
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::RestoreNativeSnapshot {
                    snapshot_id,
                    expected_revision,
                    store,
                }
            }
            NativeFileJobStartRequest::ExportCompatibleLocalBackup {
                target,
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
                NativeFileJobTask::ExportCompatibleLocalBackup {
                    target,
                    destination,
                    expected_revision,
                    store,
                }
            }
            NativeFileJobStartRequest::ExportCharacterCharx {
                portrait_rendition,
                destination,
                expected_revision,
                character_id,
                container,
                card,
                module,
            } => {
                let card = card.ok_or_else(|| {
                    NativeJobError::new(
                        "invalid-input",
                        "character CharX export requires card metadata",
                    )
                })?;
                let module = module.ok_or_else(|| {
                    NativeJobError::new(
                        "invalid-input",
                        "character CharX export requires module metadata",
                    )
                })?;
                if portrait_rendition.is_some() && container != CharacterCharxContainer::AppendedCharxJpeg {
                    return Err(error::invalid_input("plain CharX does not use a portrait rendition"));
                }
                let portrait_rendition = portrait_rendition.map(|value| value.read(&self.root)).transpose()?;
                let destination = destination.map(PathBuf::from);
                if let Some(destination) = destination.as_deref() {
                    validate_desktop_destination(destination)?;
                }
                if character_id.is_empty() || character_id.len() > 512 {
                    return Err(NativeJobError::new(
                        "invalid-input",
                        "character export requires a bounded character ID",
                    ));
                }
                let metadata_bytes = serde_json::to_vec(&(&card, &module))
                    .map_err(|error| NativeJobError::new("invalid-input", error.to_string()))?;
                if metadata_bytes.len() > content::JSON_CARD_MAX_METADATA_BYTES {
                    return Err(NativeJobError::too_large(
                        "card",
                        "character export metadata exceeds the 128 MiB limit",
                    ));
                }
                let prepared =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        store.prepare_risu_save_export(expected_revision)
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::ExportCharacterCharx {
                    portrait_rendition,
                    destination,
                    character_id,
                    card,
                    module,
                    container,
                    prepared,
                }
            }
            NativeFileJobStartRequest::ExportCharacterCard {
                portrait_rendition,
                destination,
                expected_revision,
                character_id,
                format,
                metadata,
            } => {
                if portrait_rendition.is_some() && format != CharacterCardExportFormat::PngCard {
                    return Err(error::invalid_input("JSON cards do not use a portrait rendition"));
                }
                let portrait_rendition = portrait_rendition.map(|value| value.read(&self.root)).transpose()?;
                let destination = destination.map(PathBuf::from);
                if let Some(destination) = destination.as_deref() {
                    validate_desktop_destination(destination)?;
                }
                if character_id.is_empty() || character_id.len() > 512 {
                    return Err(NativeJobError::new(
                        "invalid-input",
                        "character export requires a bounded character ID",
                    ));
                }
                let metadata_bytes = serde_json::to_vec(&metadata)
                    .map_err(|error| NativeJobError::new("invalid-input", error.to_string()))?;
                if metadata_bytes.len() > content::JSON_CARD_MAX_METADATA_BYTES {
                    return Err(NativeJobError::too_large(
                        "card",
                        "character export metadata exceeds the 128 MiB limit",
                    ));
                }
                let prepared =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        store.prepare_risu_save_export(expected_revision)
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::ExportCharacterCard {
                    portrait_rendition,
                    destination,
                    character_id,
                    format,
                    metadata,
                    prepared,
                }
            }
            NativeFileJobStartRequest::ExportRisuModule {
                destination,
                expected_revision,
                module_index,
            } => {
                let destination = destination.map(PathBuf::from);
                if let Some(destination) = destination.as_deref() {
                    validate_desktop_destination(destination)?;
                }
                let prepared =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        store.prepare_risu_save_export(expected_revision)
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::ExportRisuModule {
                    destination,
                    module_index,
                    prepared,
                }
            }
            NativeFileJobStartRequest::ExportDataset {
                destination,
                expected_revision,
            } => {
                let destination = destination.map(PathBuf::from);
                if let Some(destination) = destination.as_deref() {
                    validate_desktop_destination(destination)?;
                }
                let prepared =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        store.prepare_risu_save_export(expected_revision)
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::ExportDataset {
                    destination,
                    prepared,
                }
            }
            request @ (NativeFileJobStartRequest::ExportGalleryFile { .. }
                | NativeFileJobStartRequest::ExportGalleryZip { .. }) => {
                let archive = matches!(&request, NativeFileJobStartRequest::ExportGalleryZip { .. });
                let (destination, expected_revision, selection) = match request {
                    NativeFileJobStartRequest::ExportGalleryFile { destination, expected_revision, selection }
                    | NativeFileJobStartRequest::ExportGalleryZip { destination, expected_revision, selection } => (destination, expected_revision, selection),
                    _ => unreachable!(),
                };
                selection.validate(archive)?;
                let destination = destination.map(PathBuf::from);
                if let Some(path) = destination.as_deref() { validate_desktop_destination(path)?; }
                let selected_alias = if archive { None } else { Some(selection.single_alias()?) };
                let prepared = crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                    match selected_alias {
                        Some((kind, key)) => store.prepare_gallery_file_export(expected_revision, kind, key),
                        None => store.prepare_risu_save_export(expected_revision),
                    }
                }).map_err(native_store_error)?;
                NativeFileJobTask::ExportGallery { destination, prepared, selection, archive }
            }
            request @ NativeFileJobStartRequest::PrepareContentImport { .. } => {
                return self.start_content(request);
            }
            NativeFileJobStartRequest::ImportJpegAsset {
                source,
                display_name,
                destination,
                expected_revision,
            } => {
                if !is_bounded_content_display_name(&display_name) {
                    return Err(NativeJobError::new(
                        "invalid-input",
                        "JPEG asset display name is invalid",
                    ));
                }
                let opened_source = match &source {
                    JobSource::DesktopPath { .. } => Some(open_job_source(&self.root, &source)?),
                    JobSource::AndroidSpool { token } => {
                        parse_android_spool_token(token)?;
                        None
                    }
                    JobSource::SavedConflict { .. } | JobSource::AndroidSeekable { .. } | JobSource::IosScoped { .. } => return Err(invalid_source_error("Portable custody is not an upstream/content source")),
                };
                let store =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        store.open_native_job_store()
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::ImportJpegAsset {
                    opened_source,
                    source,
                    display_name,
                    destination,
                    expected_revision,
                    store,
                }
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
                    let account = crate::account_credential::export_account(&app, false)
                        .map_err(|message| NativeJobError::new("invalid-input", message))?
                        .ok_or_else(|| NativeJobError::new("invalid-input", "KEI account credential is unavailable"))?;
                    let prepared =
                        crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                            store.prepare_kei_job_upload(
                                &lease,
                                expected_revision,
                                &url,
                                &expected_account_id,
                                &token,
                                &account,
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
            NativeFileJobStartRequest::OfficialPublicationUpload {
                lease,
                expected_revision,
                account_id,
                base_url,
                replacements,
                session,
                save_date,
                credential,
            } => {
                #[cfg(feature = "native-official-publication")]
                {
                    let request = OfficialPublicationJobRequest {
                        lease,
                        expected_revision,
                        account_id,
                        base_url,
                        replacements,
                        session,
                        save_date,
                        credential,
                    };
                    publication::validate_start_request(&request)?;
                    NativeFileJobTask::OfficialPublication { request, app }
                }
                #[cfg(not(feature = "native-official-publication"))]
                {
                    let _ = (
                        lease,
                        expected_revision,
                        account_id,
                        base_url,
                        replacements,
                        session,
                        save_date,
                        credential,
                    );
                    return Err(NativeJobError::new(
                        "capability-unavailable",
                        "native official publication jobs are not compiled in this build",
                    ));
                }
            }
            NativeFileJobStartRequest::RestoreOfficialAccountAssets {
                base_url,
                credential,
                account_id,
                expected_library,
                key_count,
                concurrency,
                reauthenticated,
            } => {
                let (key_count, concurrency) = official_assets::validate_start(
                    &base_url,
                    &account_id,
                    &expected_library,
                    key_count,
                    concurrency,
                )?;
                let store =
                    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                        store.open_native_job_store()
                    })
                    .map_err(native_store_error)?;
                let revision = store.revision().map_err(native_store_error)?;
                if store.library_identity().map_err(native_store_error)? != expected_library {
                    return Err(NativeJobError::new(
                        "library-changed",
                        "The library changed before the official asset restore started",
                    ));
                }
                NativeFileJobTask::RestoreOfficialAssets {
                    request: official_assets::Request {
                        base_url,
                        credential,
                        account_id,
                        expected_library,
                        key_count,
                        concurrency,
                        reauthenticated,
                        store,
                        revision,
                        shared: official_assets::Handle::new(key_count),
                    },
                }
            }
            NativeFileJobStartRequest::PublishOfficialAccountAssets {
                base_url,
                credential,
                account_id,
                lease,
                session,
                assets,
                concurrency,
            } => {
                let concurrency = official_assets::upload::validate_publication_start(
                    &base_url,
                    &account_id,
                    &lease,
                    session.as_deref(),
                    &assets,
                    concurrency,
                )?;
                let (root, uploads) =
                    crate::persistent_store::commands::with_store(app.state(), |store| {
                        official_assets::upload::pinned_uploads(store, &lease, &assets)
                    })
                    .map_err(native_store_error)?;
                NativeFileJobTask::PublishOfficialAssets {
                    request: official_assets::upload::PublicationRequest {
                        base_url,
                        credential,
                        account_id,
                        session,
                        concurrency,
                        root,
                        uploads,
                        shared: official_assets::upload::PublicationHandle::default(),
                    },
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
        let _cleanup_operation = self.admit_cleanup_operation()?;
        if let Some(error) = self.capability_error.lock().map_err(|_| NativeJobError::new("store-error", "Native capability state is unavailable"))?.as_ref() {
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
        let admission = self
            .admission
            .file(false)
            .map_err(|code| NativeJobError::new(code, "Another library operation is running"))?;
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
                    .map(|source| source.opened)
            }
            JobSource::SavedConflict { .. } | JobSource::AndroidSeekable { .. } | JobSource::IosScoped { .. } => Err(invalid_source_error("Portable custody is not a content source")),
        };
        let opened_source = match opened_source {
            Ok(source) => source,
            Err(error) => {
                let cleanup =
                    cleanup_one_owned_directory(&self.root.join("jobs"), &owned_directory, &job_id);
                let _ = job.finish_native_failure(&error);
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
            let _admission = admission;
            let outcome = run_worker(
                || {
                    content::prepare_content(
                        opened_source,
                        &display_name,
                        &owned_directory,
                        &repository_root,
                        &job,
                    )
                },
                "native content worker panicked",
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
        let _cleanup_operation = self.admit_cleanup_operation()?;
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
            JobSource::SavedConflict { .. } => None,
            JobSource::DesktopPath { .. } => Some(open_job_source(&self.root, &source)?),
            JobSource::AndroidSpool { token } => {
                parse_android_spool_token(token)?;
                None
            }
            JobSource::AndroidSeekable { .. } | JobSource::IosScoped { .. } => return Err(invalid_source_error("Portable custody is not an upstream source")),
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
        let exclusive = matches!(
            task.kind(),
            JobKind::ExportPortableBackup
                | JobKind::ExportRawRecovery
                | JobKind::RestorePortableBackup
                | JobKind::RestoreBlockRisuSave
                | JobKind::RestoreLegacyLocalBackup
                | JobKind::RestoreOfficialAccountSnapshot
                | JobKind::RestoreNativeSnapshot
        );
        let upstream_staging = require_restore_finalization
            && matches!(task.kind(), JobKind::RestorePortableBackup | JobKind::RestoreBlockRisuSave
                | JobKind::RestoreLegacyLocalBackup | JobKind::RestoreOfficialAccountSnapshot
                | JobKind::RestoreNativeSnapshot);
        let admission = if upstream_staging {
            self.admission.staging()
        } else {
            self.admission.file(exclusive)
        }.map_err(|code| NativeJobError::new(code, "Another library operation is running"))?;
        let admission = Arc::new(Mutex::new(Some(admission)));
        if let NativeFileJobTask::ExportRawRecovery { app, .. } = &task {
            app.state::<crate::NativeStartupState>()
                .ensure_ready()
                .map_err(|_| NativeJobError::new(
                    "capability-unavailable",
                    "Recovery export is unavailable because native setup failed",
                ))?;
            let capture_guard = app
                .state::<crate::persistent_store::commands::PersistentStoreState>()
                .acquire_raw_capture()
                .map_err(native_store_error)?;
            if let NativeFileJobTask::ExportRawRecovery { capture_guard: slot, .. } = &mut task {
                *slot = Some(capture_guard);
            }
        }
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
                task.expected_revision(),
                warning_codes.clone(),
                require_restore_finalization
                    && matches!(
                        kind,
                        JobKind::RestoreBlockRisuSave
                            | JobKind::RestorePortableBackup
                            | JobKind::RestoreOfficialAccountSnapshot
                            | JobKind::RestoreLegacyLocalBackup
                            | JobKind::RestoreNativeSnapshot
                    ),
            )
            .map_err(|error| NativeJobError::new("store-error", error))?;
        if upstream_staging {
            job.set_activation_permit(Arc::downgrade(&admission))
                .map_err(|error| NativeJobError::new("store-error", error))?;
        }
        if let NativeFileJobTask::RestoreOfficialAssets { request } = &task {
            job.status.lock().map_err(|_| NativeJobError::new("store-error", "native job status mutex poisoned"))?
                .official_asset_shared = Some(request.shared.clone());
        }
        if let NativeFileJobTask::PublishOfficialAssets { request } = &task {
            job.status.lock().map_err(|_| NativeJobError::new("store-error", "native job status mutex poisoned"))?
                .official_asset_publication_shared = Some(request.shared.clone());
        }
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

            let (opened_source, source, expected_display_name) = match &mut task {
                NativeFileJobTask::Restore {
                    opened_source,
                    source,
                    ..
                }
                | NativeFileJobTask::RestorePortable {
                    opened_source,
                    source,
                    ..
                }
                | NativeFileJobTask::RestoreLegacyLocalBackup {
                    opened_source,
                    source,
                    ..
                } => (opened_source, source, None),
                NativeFileJobTask::ImportJpegAsset {
                    opened_source,
                    source,
                    display_name,
                    ..
                } => (opened_source, source, Some(display_name.as_str())),
                _ => return Ok(()),
            };
            match source {
                JobSource::DesktopPath { .. } if opened_source.is_some() => Ok(()),
                JobSource::AndroidSpool { token } if opened_source.is_none() => {
                    let source = claim_spool_source_with_display_name(
                        &self.root,
                        token,
                        &owned_directory,
                        expected_display_name,
                    )?;
                    *opened_source = Some(source.opened);
                    Ok(())
                }
                JobSource::SavedConflict { .. } | JobSource::AndroidSeekable { .. } | JobSource::IosScoped { .. } if opened_source.is_none() => {
                    *opened_source = Some(portable_source_custody::claim(source, &job_id)?);
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
            let _ = job.finish_native_failure(&error);
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
            let outcome = run_worker(
                || match task {
                    NativeFileJobTask::ExportPortable {
                        destination,
                        expected_revision,
                        store,
                        selection,
                        app,
                        ..
                    } => match (expected_revision, store) {
                        (Some(revision), Some(store)) => portable::export_portable(
                            destination.as_deref(),
                            revision,
                            &owned_directory,
                            &root.join("handoffs"),
                            store,
                            &job,
                            Some((&app, &selection)),
                            &app.package_info().version.to_string(),
                        ),
                        _ => Err(NativeJobError::new(
                            "store-error",
                            "portable export source was not prepared",
                        )),
                    },
                    NativeFileJobTask::ExportRawRecovery {
                        destination,
                        data_root,
                        app_version,
                        capture_guard,
                        ..
                    } => {
                        let captured = raw_recovery::capture(
                            &data_root,
                            &owned_directory,
                            &app_version,
                            &job,
                        );
                        drop(capture_guard);
                        drop(admission.lock().map_err(|_| NativeJobError::new(
                            "store-error", "Native admission slot is unavailable",
                        ))?.take());
                        captured.and_then(|captured| {
                            raw_recovery::publish(
                                captured,
                                &owned_directory,
                                &root.join("handoffs"),
                                destination.as_deref(),
                                &job,
                            )
                        })
                    }
                    NativeFileJobTask::RestorePortable {
                        opened_source,
                        source,
                        expected_revision,
                        store,
                        selection,
                        app,
                    } => match opened_source {
                        Some(input) => portable::restore_portable(
                            input,
                            matches!(source, JobSource::AndroidSpool { .. }),
                            expected_revision,
                            &owned_directory,
                            store,
                            &job,
                            Some((&app, selection.as_ref())),
                        ),
                        _ => Err(NativeJobError::new(
                            "store-error",
                            "portable input was not prepared",
                        )),
                    },
                    NativeFileJobTask::Restore {
                        opened_source,
                        expected_revision,
                        sink,
                        ..
                    } => match opened_source {
                        Some(opened_source) => match sink {
                            RestoreJobSink::Persistent(app) => restore::restore_block_risu_save(
                                opened_source,
                                &owned_directory,
                                expected_revision,
                                &job,
                                &PersistentReplacementSink { app },
                            ),
                            #[cfg(test)]
                            RestoreJobSink::Test(sink) => restore::restore_block_risu_save(
                                opened_source,
                                &owned_directory,
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
                    } => crate::account_credential::export_account(&app, omit_account)
                    .map_err(|message| NativeJobError::new("invalid-input", message))
                    .and_then(|account| {
                        let prepared = crate::persistent_store::commands::with_store_mut(app.state(), |store| {
                            store.prepare_risu_save_export(expected_revision)
                        }).map_err(native_store_error)?;
                        job.publish_export_capture(expected_revision).map_err(|message| NativeJobError::new("store-error", message))?;
                        export::export_block_risu_save(prepared, &destination, omit_account, account.as_ref(), &job)
                    }),
                    NativeFileJobTask::RestoreNativeSnapshot {
                        snapshot_id,
                        expected_revision,
                        store,
                    } => snapshot_restore::restore_native_snapshot(
                        &snapshot_id,
                        expected_revision,
                        store,
                        &job,
                    ),
                    NativeFileJobTask::RestoreOfficialSnapshot {
                        request,
                        expected_revision,
                        app,
                    } => official_snapshot::restore_official_snapshot(
                        request,
                        expected_revision,
                        &owned_directory,
                        &job,
                        &PersistentReplacementSink { app },
                    ),
                    NativeFileJobTask::RestoreLegacyLocalBackup {
                        opened_source,
                        expected_revision,
                        repository_root,
                        app,
                        ..
                    } => match opened_source {
                        Some(opened_source) => legacy_backup::restore_legacy_local_backup(
                            opened_source,
                            expected_revision,
                            &owned_directory,
                            &repository_root,
                            app,
                            &job,
                        ),
                        None => Err(NativeJobError::new(
                            "store-error",
                            "native legacy backup job source was not prepared",
                        )),
                    },
                    NativeFileJobTask::ExportCompatibleLocalBackup {
                        target,
                        destination,
                        expected_revision,
                        store,
                    } => legacy_backup::export_compatible_local_backup(
                        target,
                        destination.as_deref(),
                        expected_revision,
                        &owned_directory,
                        &root.join("handoffs"),
                        store,
                        &job,
                    ),
                    NativeFileJobTask::ExportCharacterCharx {
                        portrait_rendition,
                        destination,
                        character_id,
                        card,
                        module,
                        container,
                        prepared,
                    } => character_charx_export::export_character_charx_container(
                        prepared,
                        &character_id,
                        card,
                        module,
                        container,
                        &owned_directory,
                        &root.join("handoffs"),
                        destination.as_deref(),
                        &job,
                        portrait_rendition.as_ref(),
                    ),
                    NativeFileJobTask::ExportCharacterCard {
                        portrait_rendition,
                        destination,
                        character_id,
                        format,
                        metadata,
                        prepared,
                    } => match format {
                        CharacterCardExportFormat::JsonCard => {
                            character_json_export::export_character_json(
                                prepared,
                                &character_id,
                                metadata,
                                &owned_directory,
                                &root.join("handoffs"),
                                destination.as_deref(),
                                &job,
                            )
                        }
                        CharacterCardExportFormat::PngCard => {
                            character_png_export::export_character_png(
                                prepared,
                                &character_id,
                                metadata,
                                &owned_directory,
                                &root.join("handoffs"),
                                destination.as_deref(),
                                &job,
                                portrait_rendition.as_ref(),
                            )
                        }
                    },
                    NativeFileJobTask::ExportRisuModule {
                        destination,
                        module_index,
                        prepared,
                    } => risum_export::export_risu_module(
                        prepared,
                        module_index,
                        &owned_directory,
                        &root.join("handoffs"),
                        destination.as_deref(),
                        &job,
                    ),
                    NativeFileJobTask::ExportGallery { destination, prepared, selection, archive } =>
                        gallery_export::export_gallery(prepared, selection, archive, &owned_directory,
                            &root.join("handoffs"), destination.as_deref(), &job),
                    NativeFileJobTask::ExportDataset {
                        destination,
                        prepared,
                    } => dataset_export::export_dataset(
                        prepared,
                        &owned_directory,
                        &root.join("handoffs"),
                        destination.as_deref(),
                        &job,
                    ),
                    NativeFileJobTask::ImportJpegAsset {
                        opened_source,
                        display_name,
                        destination,
                        expected_revision,
                        store,
                        ..
                    } => match opened_source {
                        Some(opened_source) => {
                            let repository_root = store.repository_root().to_path_buf();
                            jpeg_asset::import_jpeg_asset(
                                opened_source,
                                &display_name,
                                destination,
                                expected_revision,
                                &repository_root,
                                store,
                                &job,
                            )
                        }
                        None => Err(NativeJobError::new(
                            "store-error",
                            "native JPEG asset source was not prepared",
                        )),
                    },
                    #[cfg(feature = "native-kei-upload-pilot")]
                    NativeFileJobTask::KeiBackup { prepared } => {
                        crate::persistent_store::kei::run_job(prepared, Arc::clone(&job))
                    }
                    #[cfg(feature = "native-official-publication")]
                    NativeFileJobTask::OfficialPublication { request, app } => {
                        publication::run_job(request, app, Arc::clone(&job))
                    }
                    NativeFileJobTask::RestoreOfficialAssets { request } => {
                        official_assets::run(request, &owned_directory, &job)
                    }
                    NativeFileJobTask::PublishOfficialAssets { request } => {
                        official_assets::upload::publish(request, &job)
                    }
                },
                "native file job worker panicked",
            );
            let mut cleanup_errors = Vec::new();
            if let Err(error) =
                cleanup_one_owned_directory(&root.join("jobs"), &owned_directory, &job.id())
            {
                cleanup_errors.push(error);
            }
            release_worker_admission(&admission);
            finish_worker_outcome(&job, kind, outcome, cleanup_errors, Some(&root));
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

    fn status_summary(&self, job_id: &str) -> Result<JobStatus, NativeJobError> {
        self.registry.prune().map_err(|error| NativeJobError::new("store-error", error))?;
        self.registry.lookup(job_id)
            .map_err(|error| NativeJobError::new("store-error", error))?
            .map(|job| job.status_summary())
            .ok_or_else(|| NativeJobError::new("store-error", "native job not found"))
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

    pub(crate) fn prepared_content_receipt(
        &self,
        job_id: &str,
    ) -> Result<PreparedContent, NativeJobError> {
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
        let prepared = status.prepared_content.clone().ok_or_else(|| {
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
        Ok(prepared)
    }

    pub(crate) fn list(&self) -> Result<Vec<JobStatus>, NativeJobError> {
        self.registry
            .list()
            .map_err(|error| NativeJobError::new("store-error", error))
    }

    pub(crate) fn finalize(
        &self,
        job_id: &str,
        expected_revision: Option<i64>,
    ) -> Result<FinalizeOutcome, NativeJobError> {
        self.registry
            .finalize(job_id, expected_revision)
            .map_err(|error| {
                let code = if error == "library-operation-busy" {
                    "library-operation-busy"
                } else {
                    "store-error"
                };
                NativeJobError::new(code, error)
            })
    }

    pub(crate) fn cancel(&self, job_id: &str) -> Result<CancelOutcome, NativeJobError> {
        self.registry
            .cancel(job_id)
            .map_err(|error| NativeJobError::new("store-error", error))
    }

    pub(crate) fn retry_official_publication(
        &self,
        request: OfficialPublicationRetryRequest,
    ) -> Result<(), NativeJobError> {
        #[cfg(not(feature = "native-official-publication"))]
        {
            let _ = request;
            return Err(NativeJobError::new(
                "capability-unavailable",
                "native official publication jobs are not compiled in this build",
            ));
        }
        #[cfg(feature = "native-official-publication")]
        {
            publication::validate_retry_request(&request)?;
            validate_official_publication_retry(&request)
                .map_err(|error| NativeJobError::new("invalid-input", error))?;
            self.registry
                .retry_official_publication(request)
                .map_err(|error| NativeJobError::new("invalid-input", error))
        }
    }

    pub(crate) fn forget(&self, job_id: &str) -> Result<bool, NativeJobError> {
        #[cfg(feature = "native-official-publication")]
        {
            let job = self
                .registry
                .lookup(job_id)
                .map_err(|error| NativeJobError::new("store-error", error))?;
            let Some(job) = job else {
                return Ok(false);
            };
            let status = job.status_summary();
            if status.kind == JobKind::OfficialPublicationUpload && status.state.is_terminal() {
                use crate::asset_repository::job_pins::{CasReleaseOutcome, DurableCasJob};

                let outcome = match status
                    .result
                    .as_ref()
                    .and_then(|result| result.publication.as_ref())
                {
                    Some(
                        OfficialPublicationAttemptResult::Written { .. }
                        | OfficialPublicationAttemptResult::NotModified { .. },
                    ) if status.state == JobState::Succeeded => CasReleaseOutcome::Committed,
                    _ => CasReleaseOutcome::Aborted,
                };
                let repository_root = self.root.parent().ok_or_else(|| {
                    NativeJobError::new(
                        "cleanup-failed",
                        "native job root has no repository parent",
                    )
                })?;
                match DurableCasJob::open(repository_root, job_id) {
                    Ok(mut durable) => durable.release(outcome).map_err(|error| {
                        NativeJobError::new(
                            "cleanup-failed",
                            format!("official publication CAS release failed: {error}"),
                        )
                    })?,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::NotFound
                            && status.state != JobState::Succeeded => {}
                    Err(error) => {
                        return Err(NativeJobError::new(
                            "cleanup-failed",
                            format!("official publication CAS release failed: {error}"),
                        ));
                    }
                }
            }
        }
        if let Some(job) = self.registry.lookup(job_id).map_err(|error| NativeJobError::new("store-error", error))? {
            let status = job.status();
            if status.kind == JobKind::ExportPortableBackup && status.state == JobState::Succeeded
                && status.result.as_ref().is_some_and(|result| result.handoff_path.is_some()) {
                portable_export_receipts::forget(&self.root, job_id)
                    .map_err(|error| NativeJobError::new("cleanup-failed", error))?;
            }
        }
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
    ExportPortable {
        destination: Option<PathBuf>,
        expected_revision: Option<i64>,
        store: Option<crate::persistent_store::PersistentStore>,
        source: Option<JobSource>,
        selection: portable::PortableSelection,
        app: AppHandle,
    },
    ExportRawRecovery {
        destination: Option<PathBuf>,
        data_root: PathBuf,
        app_version: String,
        app: AppHandle,
        capture_guard: Option<crate::persistent_store::commands::DeviceMaintenanceGuard>,
    },
    RestorePortable {
        opened_source: Option<OpenedJobSource>,
        source: JobSource,
        expected_revision: i64,
        store: crate::persistent_store::PersistentStore,
        selection: Option<portable::PortableSelection>,
        app: AppHandle,
    },
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
    RestoreOfficialSnapshot {
        request: official_snapshot::OfficialSnapshotRestoreRequest,
        expected_revision: i64,
        app: AppHandle,
    },
    RestoreNativeSnapshot {
        snapshot_id: String,
        expected_revision: i64,
        store: crate::persistent_store::PersistentStore,
    },
    RestoreLegacyLocalBackup {
        opened_source: Option<OpenedJobSource>,
        source: JobSource,
        expected_revision: i64,
        repository_root: PathBuf,
        app: AppHandle,
    },
    ExportCompatibleLocalBackup {
        target: legacy_backup::CompatibilityTarget,
        destination: Option<PathBuf>,
        expected_revision: i64,
        store: crate::persistent_store::PersistentStore,
    },
    ExportCharacterCharx {
        portrait_rendition: Option<portrait_rendition::PortraitRendition>,
        destination: Option<PathBuf>,
        character_id: String,
        card: Value,
        module: Value,
        container: CharacterCharxContainer,
        prepared: crate::persistent_store::PreparedRisuSaveExport,
    },
    ExportCharacterCard {
        portrait_rendition: Option<portrait_rendition::PortraitRendition>,
        destination: Option<PathBuf>,
        character_id: String,
        format: CharacterCardExportFormat,
        metadata: Value,
        prepared: crate::persistent_store::PreparedRisuSaveExport,
    },
    ExportRisuModule {
        destination: Option<PathBuf>,
        module_index: u64,
        prepared: crate::persistent_store::PreparedRisuSaveExport,
    },
    ExportDataset {
        destination: Option<PathBuf>,
        prepared: crate::persistent_store::PreparedRisuSaveExport,
    },
    ExportGallery {
        destination: Option<PathBuf>,
        prepared: crate::persistent_store::PreparedRisuSaveExport,
        selection: gallery_export::GallerySelection,
        archive: bool,
    },
    ImportJpegAsset {
        opened_source: Option<OpenedJobSource>,
        source: JobSource,
        display_name: String,
        destination: jpeg_asset::JpegAssetDestination,
        expected_revision: i64,
        store: crate::persistent_store::PersistentStore,
    },
    #[cfg(feature = "native-kei-upload-pilot")]
    KeiBackup {
        prepared: crate::persistent_store::kei::PreparedKeiUpload,
    },
    #[cfg(feature = "native-official-publication")]
    OfficialPublication {
        request: OfficialPublicationJobRequest,
        app: AppHandle,
    },
    RestoreOfficialAssets {
        request: official_assets::Request,
    },
    PublishOfficialAssets {
        request: official_assets::upload::PublicationRequest,
    },
}

impl NativeFileJobTask {
    fn kind(&self) -> JobKind {
        match self {
            Self::ExportPortable { .. } => JobKind::ExportPortableBackup,
            Self::ExportRawRecovery { .. } => JobKind::ExportRawRecovery,
            Self::RestorePortable { .. } => JobKind::RestorePortableBackup,
            Self::Restore { .. } => JobKind::RestoreBlockRisuSave,
            Self::Export { .. } => JobKind::ExportBlockRisuSave,
            Self::RestoreOfficialSnapshot { .. } => JobKind::RestoreOfficialAccountSnapshot,
            Self::RestoreNativeSnapshot { .. } => JobKind::RestoreNativeSnapshot,
            Self::RestoreLegacyLocalBackup { .. } => JobKind::RestoreLegacyLocalBackup,
            Self::ExportCompatibleLocalBackup { .. } => JobKind::ExportCompatibleLocalBackup,
            Self::ExportCharacterCharx { .. } => JobKind::ExportCharacterCharx,
            Self::ExportCharacterCard { .. } => JobKind::ExportCharacterCard,
            Self::ExportRisuModule { .. } => JobKind::ExportRisuModule,
            Self::ExportDataset { .. } => JobKind::ExportDataset,
            Self::ExportGallery { archive, .. } => if *archive { JobKind::ExportGalleryZip } else { JobKind::ExportGalleryFile },
            Self::ImportJpegAsset { .. } => JobKind::ImportJpegAsset,
            #[cfg(feature = "native-kei-upload-pilot")]
            Self::KeiBackup { .. } => JobKind::KeiBackupUpload,
            #[cfg(feature = "native-official-publication")]
            Self::OfficialPublication { .. } => JobKind::OfficialPublicationUpload,
            Self::RestoreOfficialAssets { .. } => JobKind::RestoreOfficialAccountAssets,
            Self::PublishOfficialAssets { .. } => JobKind::PublishOfficialAccountAssets,
        }
    }

    fn expected_revision(&self) -> Option<i64> {
        match self {
            Self::ExportPortable {
                expected_revision, ..
            } => *expected_revision,
            Self::ExportRawRecovery { .. } => None,
            Self::RestorePortable {
                expected_revision, ..
            } => Some(*expected_revision),
            Self::Restore {
                expected_revision, ..
            }
            | Self::Export {
                expected_revision, ..
            }
            | Self::RestoreOfficialSnapshot {
                expected_revision, ..
            }
            | Self::RestoreNativeSnapshot {
                expected_revision, ..
            }
            | Self::RestoreLegacyLocalBackup {
                expected_revision, ..
            }
            | Self::ExportCompatibleLocalBackup {
                expected_revision, ..
            }
            | Self::ImportJpegAsset {
                expected_revision, ..
            } => Some(*expected_revision),
            Self::ExportCharacterCharx { prepared, .. }
            | Self::ExportCharacterCard { prepared, .. }
            | Self::ExportRisuModule { prepared, .. }
            | Self::ExportDataset { prepared, .. }
            | Self::ExportGallery { prepared, .. } => Some(prepared.revision),
            #[cfg(feature = "native-kei-upload-pilot")]
            Self::KeiBackup { prepared } => Some(prepared.revision()),
            #[cfg(feature = "native-official-publication")]
            Self::OfficialPublication { request, .. } => Some(request.expected_revision),
            Self::RestoreOfficialAssets { .. } | Self::PublishOfficialAssets { .. } => None,
        }
    }
}

fn native_store_error(error: crate::persistent_store::StoreError) -> NativeJobError {
    let code = match error {
        crate::persistent_store::StoreError::RevisionConflict { .. } => "revision-conflict",
        crate::persistent_store::StoreError::CommitDecode { .. } | crate::persistent_store::StoreError::Validation { .. } => "invalid-input",
        crate::persistent_store::StoreError::Committed { .. } | crate::persistent_store::StoreError::RawBodyUnavailable | crate::persistent_store::StoreError::CommitBusy | crate::persistent_store::StoreError::SnapshotReleased
        | crate::persistent_store::StoreError::SchemaMismatch { .. } | crate::persistent_store::StoreError::Store { .. } => "store-error",
    };
    NativeJobError::new(code, error.to_string())
}

fn job_transition(
    job: &JobControl,
    transition: Result<(), String>,
) -> Result<(), NativeJobError> {
    transition.map_err(|error| {
        if job.is_cancel_requested() {
            NativeJobError::new("cancelled", "Native file job was cancelled")
        } else {
            NativeJobError::new("store-error", error)
        }
    })
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

fn run_worker<T>(
    worker: impl FnOnce() -> Result<T, NativeJobError>,
    panic_message: &str,
) -> Result<T, NativeJobError> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(worker))
        .unwrap_or_else(|_| Err(NativeJobError::new("store-error", panic_message)))
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

fn release_worker_admission(slot: &Mutex<Option<admission::Permit>>) {
    let mut owned = slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    drop(owned.take());
}

fn finish_worker_outcome(
    job: &JobControl,
    kind: JobKind,
    outcome: Result<JobResultSummary, NativeJobError>,
    cleanup_errors: Vec<String>,
    receipt_root: Option<&Path>,
) {
    let finish_success = |result| {
        let completed = job.finish_success_with_receipt(result, receipt_root);
        if kind == JobKind::ExportPortableBackup {
            if let Err(error) = completed {
                if job.is_cancel_requested() { let _ = job.finish_cancelled(); }
                else { let _ = job.finish_native_failure(&error); }
            }
        }
    };
    match (outcome, cleanup_errors.is_empty()) {
        (Ok(result), true) => {
            let _ = if kind == JobKind::OfficialPublicationUpload {
                job.finish_official_publication_success(result)
            } else {
                finish_success(result);
                Ok(())
            };
        }
        (Ok(mut result), false) => {
            if !result
                .warning_codes
                .iter()
                .any(|code| code == "cleanup-failed")
            {
                result.warning_codes.push("cleanup-failed".to_owned());
            }
            let _ = if kind == JobKind::OfficialPublicationUpload {
                job.finish_official_publication_success(result)
            } else {
                finish_success(result);
                Ok(())
            };
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
            let _ = job.finish_native_failure(&error);
        }
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
    fn prepare_activation_receipt(&self, staging_id: &str, job: &JobControl, result: &JobResultSummary) -> crate::persistent_store::StoreResult<Option<String>> {
        crate::persistent_store::commands::with_store(self.app.state(), |store| restore_receipts::prepare(store, job, staging_id, result))
    }

    fn activation_receipt(&self, staging_id: &str, authority: &str) -> crate::persistent_store::StoreResult<Option<i64>> {
        crate::persistent_store::commands::with_store(self.app.state(), |store| store.lww_file_restore_receipt(staging_id, staging_id, authority))
    }
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
            store.replace_put_upstream_root(staging_id, root)
        })
    }

    fn put_legacy_root(
        &self,
        staging_id: &str,
        root: &crate::persistent_store::upstream_stream::RootSpool,
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_upstream_stream(staging_id, root)
        })
    }

    fn staged_plugin_preview(
        &self,
        staging_id: &str,
    ) -> crate::persistent_store::StoreResult<crate::persistent_store::commit::StagedPluginPreview>
    {
        crate::persistent_store::commands::with_store(self.app.state(), |store| {
            store.staged_plugin_preview(staging_id)
        })
    }

    fn put_presets(
        &self,
        staging_id: &str,
        presets: &[serde_json::Value],
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_upstream_presets(staging_id, presets)
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

    fn supports_incremental_characters(&self) -> bool {
        true
    }

    fn put_character_detail(
        &self,
        staging_id: &str,
        detail: &serde_json::Value,
        conversation_count: i64,
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_character_detail(staging_id, detail, conversation_count)
        })
    }

    fn put_conversation_row(
        &self,
        staging_id: &str,
        character_id: &str,
        configured_index: i64,
        detail: &serde_json::Value,
        recent_at: i64,
        message_count: i64,
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_put_conversation_row(
                staging_id,
                character_id,
                configured_index,
                detail,
                recent_at,
                message_count,
            )
        })
    }

    fn add_conversation_messages(
        &self,
        staging_id: &str,
        character_id: &str,
        conversation_id: &str,
        start: i64,
        messages: &[serde_json::Value],
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store.replace_add_conversation_messages(
                staging_id,
                character_id,
                conversation_id,
                start,
                messages,
            )
        })
    }

    fn preserve_active_repositories(
        &self,
        staging_id: &str,
        expected_revision: i64,
    ) -> crate::persistent_store::StoreResult<()> {
        crate::persistent_store::commands::with_store_mut(self.app.state(), |store| {
            store
                .replace_preserve_repositories(staging_id, Some(expected_revision))
                .map(|_| ())
        })
    }

    fn commit(
        &self,
        staging_id: &str,
        expected_revision: i64,
        binding_authority: Option<&str>,
    ) -> crate::persistent_store::StoreResult<crate::persistent_store::RevisionResult> {
        crate::persistent_store::commands::commit_staged_replacement(
            &self.app,
            staging_id,
            Some(expected_revision),
            binding_authority,
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
    logged("native_file_job_start", state.start(request, app))
}

#[tauri::command(async)]
pub(crate) fn native_file_job_status(
    app: AppHandle,
    state: State<'_, NativeFileJobState>,
    job_id: String,
) -> Result<JobStatus, NativeJobError> {
    let status = state.status_summary(&job_id);
    if status.as_ref().is_err() || status.as_ref().is_ok_and(|status| matches!(status.kind, JobKind::RestoreBlockRisuSave | JobKind::RestoreLegacyLocalBackup) && status.state == JobState::Failed) {
        crate::persistent_store::commands::with_store(app.state(), |store| restore_receipts::reconcile_job(store, &state.registry, &job_id))
            .map_err(|error| NativeJobError::new("activation-outcome-unresolved", error.to_string()))?;
        return state.status_summary(&job_id);
    }
    status
}

#[tauri::command(async)]
pub(crate) async fn native_file_job_prepared_content(
    app: AppHandle,
    job_id: String,
) -> Result<PreparedContent, NativeJobError> {
    logged("native_file_job_prepared_content", async move {
        tauri::async_runtime::spawn_blocking(move || {
            let _operation = app.state::<crate::persistent_store::PersistentStoreState>()
                .admit_renderer_operation().map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
            crate::asset_repository::commands::with_content_session(&app, &job_id, |_| {
                app.state::<NativeFileJobState>().prepared_content_receipt(&job_id)
            })
        }).await.map_err(|error| NativeJobError::new("store-error", error.to_string()))?
    }.await)
}

#[tauri::command(async)]
pub(crate) async fn native_file_job_stage_inline_asset(
    app: AppHandle,
    job_id: String,
    name: String,
    total_bytes: u64,
    offset: u64,
    data: Vec<u8>,
) -> Result<content::InlineAssetStageResult, NativeJobError> {
    logged("native_file_job_stage_inline_asset", async move {
        if data.len() > 64 * 1024 {
            return Err(NativeJobError::new("invalid-input", "Inline asset chunk exceeds the IPC limit"));
        }
        tauri::async_runtime::spawn_blocking(move || {
            let _operation = app.state::<crate::persistent_store::PersistentStoreState>()
                .admit_renderer_operation().map_err(|error| NativeJobError::new("store-error", error.to_string()))?;
            crate::asset_repository::commands::with_unsealed_content_session(&app, &job_id, |session| {
                app.state::<NativeFileJobState>().stage_inline_asset(&job_id, session, &name, total_bytes, offset, &data)
            })
        }).await.map_err(|error| NativeJobError::new("store-error", error.to_string()))?
    }.await)
}

#[tauri::command(async)]
pub(crate) fn native_file_job_list(
    app: AppHandle,
    state: State<'_, NativeFileJobState>,
) -> Result<Vec<JobStatus>, NativeJobError> {
    logged("native_file_job_list", (|| {
        crate::persistent_store::commands::with_store_if_ready(&app.state(), |store| restore_receipts::reconcile(store, &state.registry))
            .map_err(|error| NativeJobError::new("activation-outcome-unresolved", error.to_string()))?;
        state.list()
    })())
}

fn should_retry_snapshot_bodies(state: JobState, previous_job_id: Option<&str>) -> bool {
    previous_job_id.is_some() && matches!(state, JobState::Failed | JobState::Cancelled)
}

fn validate_snapshot_body_status(status: &JobStatus, receipt: &NativeSnapshotBodiesStarted) -> Result<i64, NativeJobError> {
    let revision: i64 = receipt.activation_revision.parse().map_err(|_| NativeJobError::new("invalid-activation-receipt", "Snapshot revision is invalid"))?;
    if revision < 0 || revision.to_string() != receipt.activation_revision || receipt.kind != JobKind::SnapshotBodies
        || status.kind != JobKind::SnapshotBodies || status.job_id != receipt.job_id
        || status.snapshot_staging_id.as_deref() != Some(receipt.staging_id.as_str())
        || status.activation_revision != Some(revision) || status.activation_authority.as_deref() != Some(receipt.binding_authority.as_str()) {
        return Err(NativeJobError::new("invalid-activation-receipt", "Snapshot body operation identity differs"));
    }
    Ok(revision)
}

#[tauri::command(async)]
pub(crate) fn native_snapshot_restore_bodies_status(app: AppHandle, state: State<'_, NativeFileJobState>, receipt: NativeSnapshotBodiesStarted) -> Result<JobStatus, NativeJobError> {
    logged("native_snapshot_restore_bodies_status", (|| {
        let status = state.status(&receipt.job_id)?;
        let revision = validate_snapshot_body_status(&status, &receipt)?;
        crate::persistent_store::commands::with_store_mut(app.state(), |store| store.validate_snapshot_body_receipt(&receipt.staging_id, revision, &receipt.binding_authority)).map_err(error::store_error)?;
        Ok(status)
    })())
}

#[tauri::command(async)]
pub(crate) fn native_snapshot_restore_bodies_start(
    app: AppHandle,
    state: State<'_, NativeFileJobState>,
    staging_id: String,
    activation_revision: String,
    binding_authority: String,
    previous_job_id: Option<String>,
) -> Result<NativeSnapshotBodiesStarted,NativeJobError> {
    logged("native_snapshot_restore_bodies_start", (|| {
        let _cleanup=state.admit_cleanup_operation()?;
        let _start=state.snapshot_body_start.lock().map_err(|_|NativeJobError::new("store-error","Snapshot body start is unavailable"))?;
        let revision:i64=activation_revision.parse().map_err(|_|NativeJobError::new("invalid-activation-receipt","Snapshot revision is invalid"))?;
        if revision<0 || revision.to_string()!=activation_revision {return Err(NativeJobError::new("invalid-activation-receipt","Snapshot revision is not canonical"));}
        crate::persistent_store::commands::with_store_mut(app.state(), |store| store.validate_snapshot_body_receipt(&staging_id, revision, &binding_authority)).map_err(error::store_error)?;
        if let Some(previous) = &previous_job_id {
            let status = state.status(previous)?;
            validate_snapshot_body_status(&status, &NativeSnapshotBodiesStarted {job_id: previous.clone(), kind: JobKind::SnapshotBodies, staging_id: staging_id.clone(), activation_revision: activation_revision.clone(), binding_authority: binding_authority.clone()})?;
        }
        let mut superseded = Vec::new();
        for status in state.registry.list().map_err(|error|NativeJobError::new("store-error",error))? {
            if status.kind==JobKind::SnapshotBodies && status.snapshot_staging_id.as_deref()==Some(staging_id.as_str()) && status.state.is_terminal() {
                superseded.push(status.job_id.clone());
            }
            if status.kind==JobKind::SnapshotBodies && status.snapshot_staging_id.as_deref()==Some(staging_id.as_str())
                && !should_retry_snapshot_bodies(status.state, previous_job_id.as_deref()) {
                if status.activation_revision!=Some(revision) || status.activation_authority.as_deref()!=Some(binding_authority.as_str()) {
                    return Err(NativeJobError::new("invalid-activation-receipt","Snapshot body operation identity differs"));
                }
                return Ok(NativeSnapshotBodiesStarted {job_id:status.job_id,kind:JobKind::SnapshotBodies,staging_id,activation_revision,binding_authority});
            }
        }
        let permit=WorkerPermit::acquire(Arc::clone(&state.active_workers),state.max_concurrent_jobs)?;
        let (mut store,plan,job)=crate::persistent_store::commands::with_store_mut(app.state(),|store|Ok(state.prepare_snapshot_bodies(store,&staging_id,revision,&binding_authority)))
            .map_err(error::store_error)??;
        for id in superseded { state.registry.forget(&id).map_err(|error| NativeJobError::new("store-error", error))?; }
        let directory=snapshot_bodies::prepare_directory(&state.root.join("jobs"),&job)?;
        let started=NativeSnapshotBodiesStarted {job_id:job.id(),kind:JobKind::SnapshotBodies,staging_id,activation_revision,binding_authority};
        #[cfg(test)] let body_scope=crate::asset_repository::body_io::capture_body_io_scope();
        tauri::async_runtime::spawn_blocking(move || {
            let _permit=permit;
            let mut operation=|| {
                #[cfg(test)] let _source_scope=crate::portable_backup::source_io::attach(&job.source_io_scope);
                let mut outcome=run_worker(||snapshot_bodies::run(&mut store,&plan,&job,&directory),"Snapshot body worker panicked");
                if let Err(failure)=cleanup_one_owned_directory(directory.parent().expect("owned job directory has a parent"),&directory,&job.id()) {
                    outcome=Err(NativeJobError::new("cleanup-failed",failure));
                }
                let _=snapshot_bodies::finish(&job,outcome);
            };
            #[cfg(test)] crate::asset_repository::body_io::with_body_io_scope(body_scope,&mut operation);
            #[cfg(not(test))] operation();
        });
        Ok(started)
    })())
}

#[tauri::command(async)]
pub(crate) fn native_portable_confirm_restore_adoption(
    state: State<'_, NativeFileJobState>,
    device: State<'_, crate::device_backup::DeviceBackupState>,
    job_id: String,
    activation_revision: String,
    binding_authority: String,
    device_session_id: String,
) -> Result<(), NativeJobError> {
    logged("native_portable_confirm_restore_adoption", state.confirm_portable_restore_adoption(&device,&job_id,&activation_revision,&binding_authority,&device_session_id))
}

#[tauri::command(async)]
pub(crate) fn native_portable_retry_restore_bodies(state:State<'_,NativeFileJobState>,request:PortableBodyRetryRequest)->Result<NativeFileJobStarted,NativeJobError> {
    logged("native_portable_retry_restore_bodies", state.retry_portable_restore_bodies(&request))
}

#[tauri::command(async)]
pub(crate) fn native_portable_select_sections(
    state: State<'_, NativeFileJobState>,
    job_id: String,
    selection: portable::PortableSelection,
) -> Result<(), NativeJobError> {
    logged("native_portable_select_sections", (|| {
        let job = state
            .registry
            .lookup(&job_id)
            .map_err(|e| NativeJobError::new("store-error", e))?
            .ok_or_else(|| {
                NativeJobError::new("job-not-found", "Portable backup job is unavailable")
            })?;
        job.select_portable_sections(selection)
            .map_err(|e| NativeJobError::new("invalid-selection", e))
    })())
}

#[tauri::command(async)]
pub(crate) fn native_plugin_values_assign(
    state: State<'_, NativeFileJobState>,
    app: AppHandle,
    job_id: String,
    assignments: Vec<crate::persistent_store::commit::StagedPluginAssignment>,
    automatic: bool,
) -> Result<(), NativeJobError> {
    logged("native_plugin_values_assign", (|| {
        let job = state
            .registry
            .lookup(&job_id)
            .map_err(|e| NativeJobError::new("store-error", e))?
            .ok_or_else(|| NativeJobError::new("job-not-found", "Import job is unavailable"))?;
        let staging_id = job
            .plugin_value_staging_id()
            .map_err(|e| NativeJobError::new("store-error", e))?
            .ok_or_else(|| {
                NativeJobError::new(
                    "invalid-selection",
                    "Import job is no longer waiting for plugin value assignment",
                )
            })?;
        crate::persistent_store::commands::with_store_mut(app.state(), |store| {
            store.assign_staged_plugin_values(&staging_id, &assignments, automatic)
        })
        .map_err(|error| NativeJobError::new("store-error", error.to_string()))
    })())
}

#[tauri::command(async)]
pub(crate) fn native_file_job_finalize(
    state: State<'_, NativeFileJobState>,
    job_id: String,
    expected_revision: Option<i64>,
) -> Result<FinalizeOutcome, NativeJobError> {
    logged("native_file_job_finalize", state.finalize(&job_id, expected_revision))
}

#[tauri::command(async)]
pub(crate) fn native_file_job_cancel(
    state: State<'_, NativeFileJobState>,
    job_id: String,
) -> Result<CancelOutcome, NativeJobError> {
    logged("native_file_job_cancel", state.cancel(&job_id))
}

#[tauri::command(async)]
pub(crate) fn native_file_job_official_publication_retry(
    state: State<'_, NativeFileJobState>,
    request: OfficialPublicationRetryRequest,
) -> Result<(), NativeJobError> {
    logged("native_file_job_official_publication_retry", state.retry_official_publication(request))
}

#[tauri::command(async)]
pub(crate) fn native_file_job_forget(
    app: AppHandle,
    state: State<'_, NativeFileJobState>,
    device: State<'_, crate::device_backup::DeviceBackupState>,
    job_id: String,
) -> Result<bool, NativeJobError> {
    logged("native_file_job_forget", (|| {
        let status = state.status_summary(&job_id).ok();
        if status.as_ref().is_some_and(|status| matches!(status.kind, JobKind::RestoreBlockRisuSave | JobKind::RestoreLegacyLocalBackup)) {
            crate::persistent_store::commands::with_store(app.state(), |store| restore_receipts::reconcile_job(store, &state.registry, &job_id))
                .map_err(|error| NativeJobError::new("activation-outcome-unresolved", error.to_string()))?;
        } else if status.is_none() {
            crate::persistent_store::commands::with_store_if_ready(&app.state(), |store| restore_receipts::reconcile_job(store, &state.registry, &job_id))
                .map_err(|error| NativeJobError::new("activation-outcome-unresolved", error.to_string()))?;
        }
        let restore = state.status_summary(&job_id).is_ok_and(|status| matches!(status.kind, JobKind::RestoreBlockRisuSave | JobKind::RestoreLegacyLocalBackup));
        forget_device_session(&state, &device, &job_id)?;
        let forgotten = state.forget(&job_id)?;
        if restore {
            crate::persistent_store::commands::with_store(app.state(), |store| restore_receipts::forget(store, &state.registry, &job_id)).map_err(error::store_error)?;
        }
        Ok(forgotten)
    })())
}

fn forget_device_session(
    state: &NativeFileJobState,
    device: &crate::device_backup::DeviceBackupState,
    job_id: &str,
) -> Result<(), NativeJobError> {
    let Some(job) = state
        .registry
        .lookup(job_id)
        .map_err(|failure| NativeJobError::new("store-error", failure))?
    else {
        return Ok(());
    };
    let status = job.status();
    if status.portable_body_retry.as_ref().is_some_and(|receipt|receipt.pending) {
        return Err(NativeJobError::new("portable-bodies-pending","Portable body retry ownership is still retained"));
    }
    if !status.state.is_terminal() {
        return Err(NativeJobError::new(
            "job-active",
            "Cannot forget an active native file job",
        ));
    }
    if let Some(id) = status.device_session_id {
        device
            .cleanup(&id)
            .map_err(|failure| NativeJobError::new("cleanup-failed", failure.to_string()))?;
    }
    Ok(())
}

#[tauri::command(async)]
pub(crate) fn native_portable_handoff_cleanup(
    state: State<'_, NativeFileJobState>,
    path: String,
) -> Result<bool, NativeJobError> {
    logged("native_portable_handoff_cleanup", cleanup_handoff_path(
        &state.root,
        Path::new(&path),
        "risunest-backup-",
        ".risunest",
        "portable backup",
    ))
}

#[tauri::command(async)]
pub(crate) fn native_raw_recovery_handoff_cleanup(
    state: State<'_, NativeFileJobState>,
    path: String,
) -> Result<bool, NativeJobError> {
    logged("native_raw_recovery_handoff_cleanup", cleanup_handoff_path(
        &state.root,
        Path::new(&path),
        "risunest-rescue-",
        ".risunest-rescue.zip",
        "raw recovery",
    ))
}

#[tauri::command(async)]
pub(crate) fn native_legacy_backup_handoff_cleanup(
    state: State<'_, NativeFileJobState>,
    path: String,
) -> Result<bool, NativeJobError> {
    logged("native_legacy_backup_handoff_cleanup", cleanup_legacy_backup_handoff_path(&state.root, Path::new(&path)))
}

#[tauri::command(async)]
pub(crate) fn native_character_charx_handoff_cleanup(
    state: State<'_, NativeFileJobState>,
    path: String,
) -> Result<bool, NativeJobError> {
    logged("native_character_charx_handoff_cleanup", cleanup_character_charx_handoff_path(&state.root, Path::new(&path)))
}

#[tauri::command(async)]
pub(crate) fn native_character_card_handoff_cleanup(
    state: State<'_, NativeFileJobState>,
    path: String,
) -> Result<bool, NativeJobError> {
    logged("native_character_card_handoff_cleanup", cleanup_character_card_handoff_path(&state.root, Path::new(&path)))
}

#[tauri::command(async)]
pub(crate) fn native_risu_module_handoff_cleanup(
    state: State<'_, NativeFileJobState>,
    path: String,
) -> Result<bool, NativeJobError> {
    logged("native_risu_module_handoff_cleanup", cleanup_risu_module_handoff_path(&state.root, Path::new(&path)))
}

#[tauri::command(async)]
pub(crate) fn native_dataset_export_handoff_cleanup(
    state: State<'_, NativeFileJobState>,
    path: String,
) -> Result<bool, NativeJobError> {
    logged("native_dataset_export_handoff_cleanup", cleanup_dataset_handoff_path(&state.root, Path::new(&path)))
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JobKind {
    SnapshotBodies,
    ExportPortableBackup,
    ExportRawRecovery,
    RestorePortableBackup,
    RestoreBlockRisuSave,
    RestoreOfficialAccountSnapshot,
    RestoreNativeSnapshot,
    RestoreLegacyLocalBackup,
    ExportBlockRisuSave,
    ExportCompatibleLocalBackup,
    ExportCharacterCharx,
    ExportCharacterCard,
    ExportRisuModule,
    ExportDataset,
    ExportGalleryFile,
    ExportGalleryZip,
    PrepareContentImport,
    ImportJpegAsset,
    KeiBackupUpload,
    OfficialPublicationUpload,
    RestoreOfficialAccountAssets,
    PublishOfficialAccountAssets,
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
    CleaningUp,
    CopyingMissingBodies,
    AwaitingBackupSelection,
    Queued,
    ReadingSource,
    AwaitingContentMapping,
    StagingDatabase,
    AwaitingActivation,
    ActivatingDatabase,
    WritingExport,
    UploadingDatabase,
    AwaitingPublicationRetry,
    FinalizingPublication,
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

/// Sub-phase of an import job, in the order the work happens. The renderer's
/// progress dialog shows one row per stage; the ordering is also what
/// `set_detail` enforces, so a job can never report an earlier stage again.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum JobStage {
    DownloadingAccountBackup,
    ReadingArchive,
    RequestingBackupKey,
    DecryptingDatabase,
    PreparingAttachments,
    ReadingDatabase,
    DecodingDatabase,
    FinalizingStaging,
    AwaitingActivation,
    Activating,
    CleaningUp,
    PreparingExport,
    DownloadingExport,
    WritingExport,
    VerifyingExport,
    PublishingDestination,
    FinalizingExport,
}

fn export_progress_stage(kind: JobKind, phase: JobPhase) -> Option<JobStage> {
    if !matches!(kind, JobKind::ExportPortableBackup | JobKind::ExportBlockRisuSave
        | JobKind::ExportCompatibleLocalBackup | JobKind::ExportCharacterCharx
        | JobKind::ExportCharacterCard | JobKind::ExportRisuModule
        | JobKind::OfficialPublicationUpload) { return None; }
    match phase {
        JobPhase::WritingExport => Some(if kind == JobKind::ExportPortableBackup { JobStage::PreparingExport } else { JobStage::WritingExport }),
        JobPhase::PublishingDestination | JobPhase::UploadingDatabase => Some(JobStage::PublishingDestination),
        JobPhase::FinalizingExport | JobPhase::FinalizingPublication => Some(JobStage::FinalizingExport),
        _ => None,
    }
}

fn reset_export_detail(status: &mut JobStatus) {
    if let Some(stage) = export_progress_stage(status.kind, status.phase) {
        status.detail = Some(JobDetail::new(stage, StageUnit::Bytes, 0, None, ImportCounts::default()));
    }
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StageUnit {
    Bytes,
    Items,
}

/// Running totals of what an import has classified so far. Every counter is
/// monotonic for the life of the job; the `*_total` fields are learned once.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportCounts {
    pub(crate) entries_read: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) entries_total: Option<u64>,
    pub(crate) assets: u64,
    pub(crate) inlays: u64,
    pub(crate) cold_storage: u64,
    pub(crate) pocket_media: u64,
    pub(crate) pocket_metadata: u64,
    pub(crate) skipped: u64,
    pub(crate) attachments_prepared: u64,
    pub(crate) characters: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) characters_total: Option<u64>,
    pub(crate) presets: u64,
    pub(crate) blocks: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobDetail {
    pub(crate) stage: JobStage,
    pub(crate) stage_completed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stage_total: Option<u64>,
    pub(crate) stage_unit: StageUnit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) current_item: Option<String>,
    pub(crate) counts: ImportCounts,
}

const MAX_CURRENT_ITEM_CHARS: usize = 120;

impl JobDetail {
    pub(crate) fn new(
        stage: JobStage,
        stage_unit: StageUnit,
        stage_completed: u64,
        stage_total: Option<u64>,
        counts: ImportCounts,
    ) -> Self {
        Self {
            stage,
            stage_completed,
            stage_total,
            stage_unit,
            current_item: None,
            counts,
        }
    }

    pub(crate) fn with_current_item(mut self, item: impl Into<String>) -> Self {
        self.current_item = Some(item.into());
        self
    }

    /// Starts the next stage while carrying the counters forward.
    pub(crate) fn advance(
        &self,
        stage: JobStage,
        stage_unit: StageUnit,
        stage_total: Option<u64>,
    ) -> Self {
        Self::new(stage, stage_unit, 0, stage_total, self.counts.clone())
    }

    fn bounded(mut self) -> Self {
        if let Some(item) = self.current_item.as_mut() {
            let chars = item.chars().count();
            if chars > MAX_CURRENT_ITEM_CHARS {
                let tail: String = item
                    .chars()
                    .skip(chars - (MAX_CURRENT_ITEM_CHARS - 1))
                    .collect();
                *item = format!("…{tail}");
            }
        }
        self
    }
}

/// Detail for the activation hand-off, carrying the counts the restore gathered.
fn activation_detail(previous: Option<&JobDetail>, stage: JobStage) -> JobDetail {
    match previous {
        Some(previous) => previous.advance(stage, StageUnit::Items, None),
        None => JobDetail::new(stage, StageUnit::Items, 0, None, ImportCounts::default()),
    }
}

fn validate_detail_transition(
    previous: Option<&JobDetail>,
    next: &JobDetail,
) -> Result<(), String> {
    if next
        .stage_total
        .is_some_and(|total| next.stage_completed > total)
    {
        return Err("native job stage progress exceeds its total".to_owned());
    }
    let Some(previous) = previous else {
        return Ok(());
    };
    if next.stage < previous.stage {
        return Err("native job stage cannot move backwards".to_owned());
    }
    // A stage total may become unknown, as when a restore starts reading
    // payloads of unknown size, but it never changes to another value.
    if next.stage == previous.stage
        && (next.stage_completed < previous.stage_completed
            || previous
                .stage_total
                .zip(next.stage_total)
                .is_some_and(|(before, after)| before != after))
    {
        return Err("native job stage progress is invalid or non-monotonic".to_owned());
    }
    let before = &previous.counts;
    let after = &next.counts;
    let counters = [
        (before.entries_read, after.entries_read),
        (before.assets, after.assets),
        (before.inlays, after.inlays),
        (before.cold_storage, after.cold_storage),
        (before.pocket_media, after.pocket_media),
        (before.pocket_metadata, after.pocket_metadata),
        (before.skipped, after.skipped),
        (before.attachments_prepared, after.attachments_prepared),
        (before.characters, after.characters),
        (before.presets, after.presets),
        (before.blocks, after.blocks),
    ];
    if counters.iter().any(|(old, new)| new < old)
        || before
            .entries_total
            .is_some_and(|total| after.entries_total != Some(total))
        || before
            .characters_total
            .is_some_and(|total| after.characters_total != Some(total))
    {
        return Err("native job import counts cannot decrease".to_owned());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PortableBodyRetry {
    pub(crate) staging_id:String,
    pub(crate) catalog_sha256:String,
    pub(crate) pending:bool,
    pub(crate) available:bool,
    pub(crate) source_required:bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all="camelCase",deny_unknown_fields)]
pub(crate) struct PortableBodyRetryRequest {
    pub(crate) job_id:String,
    pub(crate) staging_id:String,
    pub(crate) catalog_sha256:String,
    pub(crate) activation_revision:String,
    pub(crate) binding_authority:String,
    pub(crate) device_session_id:String,
}

fn verify_portable_body_receipt(repository:&Path,request:&PortableBodyRetryRequest)->Result<(),NativeJobError> {
    let valid=(||->crate::persistent_store::StoreResult<bool> {
        let flags=rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY|rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let persistent=repository.join("persistent");
        let library=crate::sqlite_open::open_with_flags(persistent.join(crate::persistent_store::DATABASE_FILE),flags)?;
        let device=crate::sqlite_open::open_with_flags(persistent.join(crate::persistent_store::device_store::DEVICE_DATABASE_FILE),flags)?;
        library.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")?;
        device.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")?;
        let generation:String=library.query_row("SELECT value FROM meta WHERE key='activeGeneration'",[],|row|row.get(0))?;
        let generation:String=serde_json::from_str(&generation)?;
        let authority:String=device.query_row("SELECT binding_authority FROM lww_clock WHERE singleton=1",[],|row|row.get(0))?;
        if authority!=request.binding_authority {return Ok(false);}
        let header=crate::persistent_store::lww::Header {request_id:request.job_id.clone(),binding_authority:serde_json::from_value(serde_json::json!(request.binding_authority))?};
        let receipt=crate::persistent_store::lww::completed_device_replacement_receipt(&library,&device,&header,&request.staging_id)?;
        let activated=crate::persistent_store::lww::activated_receipt(&library,&request.job_id)?;
        Ok(receipt.is_some_and(|receipt|receipt.revision.to_string()==request.activation_revision
            && activated.as_ref().is_some_and(|(activated_revision,activated)|*activated_revision==receipt.revision && *activated==generation)))
    })().map_err(native_store_error)?;
    if !valid {return Err(NativeJobError::new("invalid-activation-receipt","Portable activated library identity differs"));}
    Ok(())
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) portable_body_retry:Option<PortableBodyRetry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) snapshot_staging_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) snapshot_bodies: Option<snapshot_bodies::BodyResult>,
    pub(crate) job_id: String,
    pub(crate) kind: JobKind,
    pub(crate) state: JobState,
    pub(crate) phase: JobPhase,
    pub(crate) progress: JobProgress,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) detail: Option<JobDetail>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) expected_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) export_capture_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) activation_revision: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) activation_authority: Option<String>,
    pub(crate) restore_adoption_confirmed: bool,
    pub(crate) warning_codes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<JobResultSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<JobFailure>,
    #[serde(skip_serializing)]
    pub(crate) prepared_content: Option<PreparedContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) publication_attempt: Option<OfficialPublicationAttemptResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) compatibility_report: Option<legacy_backup::CompatibilityReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) preservation_report: Option<crate::portable_backup::PreservationReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) device_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) replaces_library: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) restore_preview: Option<portable::RestorePreview>,
    /// Values the staged save left without an owner, shown while the job waits
    /// for activation so a person can hand them to a plugin first.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) plugin_value_preview: Option<crate::persistent_store::commit::StagedPluginPreview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) incomplete_restore_preview: Option<IncompleteRestorePreview>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) official_asset_restore: Option<official_assets::RestoreProgress>,
    #[serde(skip_serializing)]
    pub(crate) official_asset_shared: Option<official_assets::Handle>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) official_asset_publication: Option<official_assets::upload::PublicationProgress>,
    #[serde(skip_serializing)]
    pub(crate) official_asset_publication_shared: Option<official_assets::upload::PublicationHandle>,
}

impl JobStatus {
    fn summary(&self) -> Self {
        Self {
            portable_body_retry: self.portable_body_retry.clone(),
            snapshot_staging_id: self.snapshot_staging_id.clone(),
            snapshot_bodies: self.snapshot_bodies.clone(),
            job_id: self.job_id.clone(),
            kind: self.kind,
            state: self.state,
            phase: self.phase,
            progress: self.progress.clone(),
            detail: self.detail.clone(),
            expected_revision: self.expected_revision,
            export_capture_revision: self.export_capture_revision,
            activation_revision: self.activation_revision,
            activation_authority: self.activation_authority.clone(),
            restore_adoption_confirmed: self.restore_adoption_confirmed,
            warning_codes: self.warning_codes.clone(),
            result: self.result.clone(),
            error: self.error.clone(),
            prepared_content: None,
            publication_attempt: self.publication_attempt.clone(),
            compatibility_report: self.compatibility_report.clone(),
            preservation_report: self.preservation_report.clone(),
            device_session_id: self.device_session_id.clone(),
            replaces_library: self.replaces_library,
            restore_preview: self.restore_preview.clone(),
            plugin_value_preview: self.plugin_value_preview.clone(),
            incomplete_restore_preview: self.incomplete_restore_preview.clone(),
            official_asset_restore: self.official_asset_restore.clone(),
            official_asset_shared: self.official_asset_shared.clone(),
            official_asset_publication: self.official_asset_publication.clone(),
            official_asset_publication_shared: self.official_asset_publication_shared.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IncompleteRestorePreview {
    pub(crate) unavailable_cold_keys: Vec<String>,
    pub(crate) character_names: Vec<String>,
    pub(crate) invalid_inlays: Vec<String>,
    pub(crate) omissions: RestoreOmissions,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RestoreOmissions {
    pub(crate) data: u64,
    pub(crate) remote: u64,
    pub(crate) missing: u64,
    pub(crate) values: u64,
    pub(crate) plugin_storage: u64,
    pub(crate) damaged_plugin_storage: u64,
    pub(crate) oversized_plugin_storage: u64,
    pub(crate) inlays: u64,
    pub(crate) excluded_characters: u64,
    pub(crate) missing_assets: u64,
}

impl IncompleteRestorePreview {
    fn warning_codes(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if !self.unavailable_cold_keys.is_empty() {
            warnings.push(format!("unavailable-cold-payloads:{}", self.unavailable_cold_keys.len()));
        }
        if !self.invalid_inlays.is_empty() { warnings.push("invalid-inlay-entry".to_owned()); }
        for (kind, count) in [
            ("skipped-backup-data", self.omissions.data),
            ("remote-backup-data", self.omissions.remote),
            ("missing-backup-data", self.omissions.missing),
            ("skipped-backup-values", self.omissions.values),
            ("skipped-plugin-storage", self.omissions.plugin_storage),
            ("damaged-plugin-storage", self.omissions.damaged_plugin_storage),
            ("oversized-plugin-storage", self.omissions.oversized_plugin_storage),
            ("skipped-backup-inlays", self.omissions.inlays),
            ("excluded-backup-characters", self.omissions.excluded_characters),
            ("missing-backup-assets", self.omissions.missing_assets),
        ] {
            if count != 0 { warnings.push(format!("{kind}:{count}")); }
        }
        warnings
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportExclusions {
    pub(crate) archived_characters: u64,
    pub(crate) colliding_plugin_values: u64,
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SourceFingerprintKind {
    WholeFileSha256,
    PortableCatalogSha256,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobResultSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) export_exclusions: Option<ExportExclusions>,
    pub(crate) revision: i64,
    pub(crate) source_bytes: u64,
    pub(crate) source_sha256: String,
    pub(crate) source_fingerprint_kind: SourceFingerprintKind,
    pub(crate) character_count: u64,
    pub(crate) preset_count: u64,
    pub(crate) warning_codes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) handoff_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) publication: Option<OfficialPublicationAttemptResult>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct JobFailure {
    pub(crate) code: String,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) item_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) owner: Option<FailureOwner>,
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
    #[cfg(test)]
    content_preparation_gate: Mutex<Option<Arc<ContentPreparationGate>>>,
    jobs: Mutex<HashMap<String, Arc<JobControl>>>,
    hidden_restore_receipts: Mutex<HashSet<String>>,
    max_terminal_jobs: usize,
    max_terminal_age: Duration,
}

#[derive(Default)]
struct JobWaitState {
    portable_adoption_confirmed: bool,
    portable_body_retry_requested:bool,
    portable_body_retired:bool,
    portable_body_source:Option<(File,crate::asset_repository::ExactFileIdentity)>,
    inline_asset_upload: Option<content::InlineAssetUpload>,
    portable_selection: Option<portable::PortableSelection>,
    restore_finalized: bool,
    activation_permit: Option<std::sync::Weak<Mutex<Option<admission::Permit>>>>,
    /// Revision the renderer holds its replacement fence at. The renderer is
    /// free to commit while this job reads and stages, so activation, not the
    /// start request, decides which revision the replacement applies to.
    activation_expected_revision: Option<i64>,
    official_publication_retry: Option<OfficialPublicationRetryInput>,
    /// Staging this job holds while it waits for activation, so the renderer's
    /// assignment reaches the right replacement.
    plugin_value_staging_id: Option<String>,
}

impl Default for JobRegistry {
    fn default() -> Self {
        Self::with_retention(64, Duration::from_secs(60 * 60))
    }
}

impl JobRegistry {
    fn with_retention(max_terminal_jobs: usize, max_terminal_age: Duration) -> Self {
        Self {
            #[cfg(test)]
            content_preparation_gate: Mutex::new(None),
            jobs: Mutex::new(HashMap::new()),
            hidden_restore_receipts: Mutex::default(),
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
                    | JobKind::RestorePortableBackup
                    | JobKind::RestoreOfficialAccountSnapshot
                    | JobKind::RestoreLegacyLocalBackup
                    | JobKind::RestoreNativeSnapshot
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
            #[cfg(test)]
            content_preparation_gate: self.content_preparation_gate.lock().unwrap().clone(),
            #[cfg(test)] source_io_scope: crate::portable_backup::source_io::capture_scope(),
            cancel_requested: Arc::new(AtomicBool::new(false)),
            requires_restore_finalization,
            wait_state: Mutex::new(JobWaitState::default()),
            wait_changed: Condvar::new(),
            terminal_at: Mutex::new(None),
            status: Mutex::new(JobStatus {
                portable_body_retry:None,
                snapshot_staging_id: None,
                snapshot_bodies: None,
                job_id: id.clone(),
                kind,
                state: JobState::Queued,
                phase: JobPhase::Queued,
                progress: JobProgress::default(),
                detail: None,
                expected_revision,
                export_capture_revision: None,
                activation_revision: None,
                activation_authority: None,
                restore_adoption_confirmed: false,
                warning_codes,
                result: None,
                error: None,
                prepared_content: None,
                publication_attempt: None,
                compatibility_report: None,
                preservation_report: None,
                device_session_id: None,
                replaces_library: None,
                restore_preview: None,
                plugin_value_preview: None,
                incomplete_restore_preview: None,
                official_asset_restore: None,
                official_asset_shared: None,
                official_asset_publication: None,
                official_asset_publication_shared: None,
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
        let mut statuses = jobs.into_iter().map(|job| job.status_summary()).collect::<Vec<_>>();
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

    pub(crate) fn finalize(
        &self,
        id: &str,
        expected_revision: Option<i64>,
    ) -> Result<FinalizeOutcome, String> {
        self.prune()?;
        let Some(job) = self.lookup(id)? else {
            return Ok(FinalizeOutcome::Missing);
        };
        job.request_finalize(expected_revision)
    }

    pub(crate) fn retry_official_publication(
        &self,
        request: OfficialPublicationRetryRequest,
    ) -> Result<(), String> {
        self.prune()?;
        let job = self
            .lookup(&request.job_id)?
            .ok_or_else(|| "native job not found".to_owned())?;
        job.request_official_publication_retry(request)
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
        let status = job.status_summary();
        if !status.state.is_terminal() {
            return Err("native job cannot be forgotten before it is terminal".to_owned());
        }
        if status.portable_body_retry.is_some_and(|receipt|receipt.pending) {
            return Err("portable body retry ownership is still retained".into());
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
            if job.retains_terminal_receipt_until_forget() {
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

#[cfg(test)]
struct ContentPreparationGate {
    entered: std::sync::mpsc::SyncSender<()>,
    resume: Mutex<std::sync::mpsc::Receiver<()>>,
}

pub(crate) struct JobControl {
    #[cfg(test)]
    content_preparation_gate: Option<Arc<ContentPreparationGate>>,
    #[cfg(test)] pub(super) source_io_scope: crate::portable_backup::source_io::Scope,
    cancel_requested: Arc<AtomicBool>,
    requires_restore_finalization: bool,
    wait_state: Mutex<JobWaitState>,
    wait_changed: Condvar,
    terminal_at: Mutex<Option<Instant>>,
    status: Mutex<JobStatus>,
}

impl crate::local_backup::CancellationProbe for JobControl {
    fn cancellation_flag(&self) -> Option<Arc<AtomicBool>> {
        Some(JobControl::cancellation_flag(self))
    }
    fn is_cancelled(&self) -> bool {
        self.is_cancel_requested()
    }
}

impl JobControl {
    pub(crate) fn publish_file_activation(&self, result: &JobResultSummary, authority: String) -> Result<(), String> {
        let mut status = self.status.lock().map_err(|_| "native job mutex poisoned".to_owned())?;
        if !matches!(status.kind, JobKind::RestoreBlockRisuSave | JobKind::RestoreLegacyLocalBackup)
            || status.phase != JobPhase::ActivatingDatabase || status.state.is_terminal() {
            return Err("invalid file restore activation receipt".into());
        }
        status.activation_revision = Some(result.revision);
        status.activation_authority = Some(authority);
        status.result = Some(result.clone());
        status.phase = JobPhase::CleaningUp;
        if let Some(detail) = status.detail.as_mut() {
            detail.stage = JobStage::CleaningUp;
            detail.stage_completed = 0;
            detail.stage_total = None;
            detail.stage_unit = StageUnit::Items;
            detail.current_item = None;
        }
        Ok(())
    }
    pub(crate) fn publish_export_capture(&self, revision: i64) -> Result<(), String> {
        let mut status = self.status.lock().map_err(|_| "native job mutex poisoned".to_owned())?;
        if !matches!(status.kind, JobKind::ExportBlockRisuSave | JobKind::ExportPortableBackup)
            || status.state.is_terminal() || status.expected_revision != Some(revision)
            || status.export_capture_revision.is_some_and(|previous| previous != revision) {
            return Err("native export capture receipt differs".into());
        }
        status.export_capture_revision = Some(revision);
        Ok(())
    }
    pub(crate) fn set_incomplete_restore_preview(&self, mut preview: IncompleteRestorePreview) -> Result<(), String> {
        let mut status = self.status.lock().map_err(|_| "native job mutex poisoned".to_owned())?;
        if let Some(previous) = &status.incomplete_restore_preview {
            let (inlays, assets) = (preview.omissions.inlays, preview.omissions.missing_assets);
            preview.omissions = previous.omissions.clone();
            preview.omissions.inlays = preview.omissions.inlays.saturating_add(inlays);
            preview.omissions.missing_assets = preview.omissions.missing_assets.saturating_add(assets);
        }
        status.incomplete_restore_preview = if preview.warning_codes().is_empty() { None } else { Some(preview) };
        Ok(())
    }

    pub(crate) fn record_restore_omission(&self, kind: &str, count: u64) -> Result<(), String> {
        let mut status = self.status.lock().map_err(|_| "native job mutex poisoned".to_owned())?;
        let omissions = &mut status.incomplete_restore_preview.get_or_insert_with(Default::default).omissions;
        let target = match kind {
            "data" => &mut omissions.data,
            "remote" => &mut omissions.remote,
            "missing" => &mut omissions.missing,
            "values" => &mut omissions.values,
            "pluginStorage" => &mut omissions.plugin_storage,
            "damagedPluginStorage" => &mut omissions.damaged_plugin_storage,
            "oversizedPluginStorage" => &mut omissions.oversized_plugin_storage,
            "inlays" => &mut omissions.inlays,
            "excludedCharacters" => &mut omissions.excluded_characters,
            _ => return Err("unknown restore omission category".to_owned()),
        };
        *target = target.saturating_add(count);
        Ok(())
    }
    /// Records what the staged save left unowned, and which staging holds it, so
    /// the renderer can assign before the replacement is applied.
    pub(crate) fn set_plugin_value_preview(
        &self,
        staging_id: &str,
        preview: crate::persistent_store::commit::StagedPluginPreview,
    ) -> Result<(), String> {
        let mut wait = self
            .wait_state
            .lock()
            .map_err(|_| "native job mutex poisoned".to_owned())?;
        wait.plugin_value_staging_id = Some(staging_id.to_owned());
        drop(wait);
        let mut status = self
            .status
            .lock()
            .map_err(|_| "native job mutex poisoned".to_owned())?;
        if status.state.is_terminal() {
            return Err("cannot change a completed job".into());
        }
        status.plugin_value_preview = if preview.values.is_empty() {
            None
        } else {
            Some(preview)
        };
        Ok(())
    }

    /// The staging a pending assignment applies to, while the job still waits.
    pub(crate) fn plugin_value_staging_id(&self) -> Result<Option<String>, String> {
        let wait = self
            .wait_state
            .lock()
            .map_err(|_| "native job mutex poisoned".to_owned())?;
        if wait.restore_finalized {
            return Ok(None);
        }
        Ok(wait.plugin_value_staging_id.clone())
    }

    fn set_preservation_report(
        &self,
        report: crate::portable_backup::PreservationReport,
    ) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|_| "native job mutex poisoned".to_owned())?;
        if status.state.is_terminal() {
            return Err("cannot change completed preservation report".into());
        }
        status.preservation_report = Some(report);
        Ok(())
    }
    pub(crate) fn publish_portable_activation(&self, revision: i64, authority: String) -> Result<(), String> {
        let mut status = self.status.lock().map_err(|_| "native job mutex poisoned".to_owned())?;
        if status.kind != JobKind::RestorePortableBackup || revision < 0 || status.state.is_terminal() {
            return Err("invalid portable activation receipt".into());
        }
        if status.activation_revision.is_some_and(|old| old != revision) {
            return Err("portable activation receipt changed".into());
        }
        if status.activation_authority.as_ref().is_some_and(|old| old != &authority) {
            return Err("portable activation authority changed".into());
        }
        status.activation_revision = Some(revision);
        status.activation_authority = Some(authority);
        Ok(())
    }

    pub(crate) fn set_snapshot_bodies(&self,result:snapshot_bodies::BodyResult)->Result<(),String> {
        let mut status=self.status.lock().map_err(|_|"native job mutex poisoned".to_owned())?;
        if status.kind!=JobKind::SnapshotBodies || status.state.is_terminal()
            || status.snapshot_staging_id.as_deref()!=Some(result.stage_id.as_str())
            || status.activation_revision!=Some(result.activated_revision)
            || status.activation_authority.as_deref()!=Some(result.binding_authority.as_str()) {
            return Err("snapshot body receipt differs".into());
        }
        status.snapshot_bodies=Some(result);
        Ok(())
    }

    fn confirm_portable_adoption(&self, revision: &str, authority: &str, session: &str, verify_completed:impl FnOnce()->Result<(),String>) -> Result<(), String> {
        let mut status = self.status.lock().map_err(|_| "native job mutex poisoned".to_owned())?;
        if status.kind != JobKind::RestorePortableBackup
            || status.activation_revision.map(|value| value.to_string()).as_deref() != Some(revision)
            || status.activation_authority.as_deref() != Some(authority)
            || status.device_session_id.as_deref() != Some(session) {
            return Err("portable adoption receipt does not match the activated operation".into());
        }
        let mut wait = self.wait_state.lock().map_err(|_| "native job wait mutex poisoned".to_owned())?;
        if wait.portable_adoption_confirmed { return Ok(()); }
        verify_completed()?;
        let slot = wait.activation_permit.as_ref().and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| "portable activation owner is unavailable".to_owned())?;
        release_worker_admission(&slot);
        wait.portable_adoption_confirmed = true;
        status.restore_adoption_confirmed = true;
        self.wait_changed.notify_all();
        Ok(())
    }

    pub(crate) fn wait_for_portable_adoption(&self) -> Result<(), String> {
        let mut wait = self.wait_state.lock().map_err(|_| "native job wait mutex poisoned".to_owned())?;
        if wait.activation_permit.is_none() { return Ok(()); }
        while !wait.portable_adoption_confirmed {
            if wait.portable_body_retired {return Err("portable-body-source-required".into());}
            wait = self.wait_changed.wait(wait).map_err(|_| "native job wait mutex poisoned".to_owned())?;
        }
        Ok(())
    }

    pub(crate) fn prepare_portable_body_retry(&self,stage:&str,catalog:&str,source:(File,crate::asset_repository::ExactFileIdentity))->Result<(),String> {
        let mut status=self.status.lock().map_err(|_|"native job mutex poisoned")?;
        if !status.restore_adoption_confirmed || status.device_session_id.is_none() {return Err("portable adoption has not been confirmed".into());}
        let mut wait=self.wait_state.lock().map_err(|_|"native job wait mutex poisoned")?;
        status.portable_body_retry=Some(PortableBodyRetry {staging_id:stage.into(),catalog_sha256:catalog.into(),pending:true,available:false,source_required:false});
        wait.portable_body_source=Some(source);
        Ok(())
    }

    pub(crate) fn wait_for_portable_body_retry(&self,failure:&NativeJobError)->Result<bool,String> {
        let mut status=self.status.lock().map_err(|_|"native job mutex poisoned")?;
        let mut wait=self.wait_state.lock().map_err(|_|"native job wait mutex poisoned")?;
        let receipt=status.portable_body_retry.as_mut().ok_or("portable body owner is missing")?;
        receipt.available=!wait.portable_body_retired;
        status.state=if self.is_cancel_requested() {JobState::Cancelled} else {JobState::Failed};
        status.error=Some(JobFailure {code:failure.code.clone(),message:bounded_message(&failure.message),item_kind:None,owner:None});
        drop(status);
        while !wait.portable_body_retry_requested && !wait.portable_body_retired {
            wait=self.wait_changed.wait(wait).map_err(|_|"native job wait mutex poisoned")?;
        }
        let retry=!wait.portable_body_retired;
        wait.portable_body_retry_requested=false;
        drop(wait);
        if !retry {self.portable_body_source_required()?;}
        Ok(retry)
    }

    fn retire_portable_body_owner(&self)->Result<(),String> {
        let mut wait=self.wait_state.lock().map_err(|_|"native job wait mutex poisoned")?;
        wait.portable_body_retired=true;
        drop(wait);
        self.wait_changed.notify_all();
        Ok(())
    }

    pub(crate) fn portable_body_source_required(&self)->Result<(),String> {
        let mut status=self.status.lock().map_err(|_|"native job mutex poisoned")?;
        if let Some(receipt)=status.portable_body_retry.as_mut() {receipt.available=false;receipt.source_required=true;}
        status.state=JobState::Running;
        self.wait_state.lock().map_err(|_|"native job wait mutex poisoned")?.portable_body_source.take();
        Ok(())
    }

    pub(crate) fn portable_bodies_completed(&self)->Result<(),String> {
        let mut status=self.status.lock().map_err(|_|"native job mutex poisoned")?;
        if let Some(receipt)=status.portable_body_retry.as_mut() {receipt.pending=false;receipt.available=false;}
        self.wait_state.lock().map_err(|_|"native job wait mutex poisoned")?.portable_body_source.take();
        Ok(())
    }

    fn retry_portable_bodies(&self,expected:&JobStatus)->Result<(),String> {
        let mut status=self.status.lock().map_err(|_|"native job mutex poisoned")?;
        if status.activation_revision!=expected.activation_revision || status.activation_authority!=expected.activation_authority || status.device_session_id!=expected.device_session_id || status.portable_body_retry!=expected.portable_body_retry {return Err("portable body receipt changed".into());}
        let mut wait=self.wait_state.lock().map_err(|_|"native job wait mutex poisoned")?;
        if wait.portable_body_retired {return Err("portable-body-source-required".into());}
        let receipt=status.portable_body_retry.as_mut().ok_or("portable body owner is missing")?;
        if !receipt.pending || !receipt.available || receipt.source_required {return Err("portable body retry is unavailable".into());}
        let (file,identity)=wait.portable_body_source.as_ref().ok_or("portable-body-source-required")?;
        if !crate::asset_repository::exact_file_identity(file).is_ok_and(|actual|actual==*identity) {
            receipt.available=false;
            receipt.source_required=true;
            wait.portable_body_retired=true;
            drop(wait);drop(status);
            self.wait_changed.notify_all();
            return Err("portable-body-source-required".into());
        }
        receipt.available=false;
        self.cancel_requested.store(false,Ordering::Release);
        wait.portable_body_retry_requested=true;
        status.state=JobState::Running;
        status.phase=JobPhase::CopyingMissingBodies;
        status.error=None;
        drop(wait);drop(status);
        self.wait_changed.notify_all();
        Ok(())
    }

    pub(crate) fn set_replaces_library(&self, replaces_library: bool) -> Result<(), String> {
        let mut status = self.status.lock().map_err(|_| "native job mutex poisoned".to_owned())?;
        if status.state.is_terminal() {
            return Err("cannot change a completed restore selection".into());
        }
        status.replaces_library = Some(replaces_library);
        Ok(())
    }

    fn wait_for_portable_selection(
        &self,
        preview: portable::RestorePreview,
    ) -> Result<portable::PortableSelection, String> {
        {
            let mut status = self
                .status
                .lock()
                .map_err(|_| "native job mutex poisoned".to_owned())?;
            if status.kind != JobKind::RestorePortableBackup
                || status.state != JobState::Running
                || status.phase != JobPhase::ReadingSource
            {
                return Err("portable preview requires reading job".into());
            }
            status.restore_preview = Some(preview);
            status.state = JobState::WaitingForInput;
            status.phase = JobPhase::AwaitingBackupSelection;
        }
        let mut wait = self
            .wait_state
            .lock()
            .map_err(|_| "native job wait mutex poisoned".to_owned())?;
        loop {
            if self.is_cancel_requested() {
                return Err("portable restore cancelled during selection".into());
            }
            if let Some(selection) = wait.portable_selection.take() {
                return Ok(selection);
            }
            wait = self
                .wait_changed
                .wait(wait)
                .map_err(|_| "native selection wait failed".to_owned())?;
        }
    }
    fn select_portable_sections(
        &self,
        selection: portable::PortableSelection,
    ) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|_| "native job mutex poisoned".to_owned())?;
        if status.state != JobState::WaitingForInput
            || status.phase != JobPhase::AwaitingBackupSelection
            || self.is_cancel_requested()
        {
            return Err("portable selection is not pending".into());
        }
        let preview = status
            .restore_preview
            .as_ref()
            .ok_or("portable preview missing")?;
        if !selection.library && selection.device_sections.is_empty()
            || selection.library && (!preview.library_included || preview.repair_required)
            || selection
                .device_sections
                .iter()
                .any(|s| !preview.device_sections.contains(s))
            || selection
                .device_sections
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                != selection.device_sections.len()
        {
            return Err("invalid portable restore selection".into());
        }
        {
            let mut wait = self
                .wait_state
                .lock()
                .map_err(|_| "native job wait mutex poisoned".to_owned())?;
            wait.portable_selection = Some(selection);
        }
        status.state = JobState::Running;
        status.phase = JobPhase::ReadingSource;
        self.wait_changed.notify_all();
        Ok(())
    }
    fn set_device_session(&self, id: &str) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|_| "native job mutex poisoned".to_owned())?;
        if !matches!(status.state, JobState::Running | JobState::Cancelling) {
            return Err("device maintenance requires running job".into());
        }
        status.device_session_id = Some(id.into());
        Ok(())
    }
    pub(crate) fn set_compatibility_report(
        &self,
        report: legacy_backup::CompatibilityReport,
    ) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state.is_terminal() {
            return Err("native job report cannot change after completion".into());
        }
        status.compatibility_report = Some(report);
        Ok(())
    }
    pub(crate) fn id(&self) -> String {
        self.status.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).job_id.clone()
    }

    fn status_summary(&self) -> JobStatus {
        self.status.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).summary()
    }

    pub(crate) fn status(&self) -> JobStatus {
        // JobStatus is plain data, so a snapshot taken from a poisoned mutex
        // is still safe to read; recovering keeps list/status/forget usable
        // after a worker-thread panic instead of panicking with it.
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn cancellation_flag(&self) -> Arc<AtomicBool> { self.cancel_requested.clone() }

    pub(crate) fn is_cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::Acquire)
    }

    fn retains_terminal_receipt_until_forget(&self) -> bool {
        let status = self
            .status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        (status.kind == JobKind::PrepareContentImport
            && status.state == JobState::Succeeded
            && status.prepared_content.is_some())
            || (status.kind == JobKind::ExportCharacterCharx && status.state.is_terminal())
            || (status.kind == JobKind::ExportCharacterCard && status.state.is_terminal())
            || (status.kind == JobKind::ExportRisuModule && status.state.is_terminal())
            || (matches!(status.kind, JobKind::ExportDataset | JobKind::ExportGalleryFile | JobKind::ExportGalleryZip) && status.state.is_terminal())
            || (status.kind == JobKind::ExportPortableBackup && status.state.is_terminal())
            || (status.kind == JobKind::SnapshotBodies && status.state.is_terminal())
            || (status.kind == JobKind::ExportCompatibleLocalBackup && status.state.is_terminal())
            || (status.device_session_id.is_some() && status.state.is_terminal())
            || (status.kind == JobKind::OfficialPublicationUpload && status.state.is_terminal())
            || (matches!(status.kind, JobKind::RestoreBlockRisuSave | JobKind::RestoreLegacyLocalBackup) && status.state.is_terminal())
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
            JobPhase::ActivatingDatabase
                | JobPhase::CleaningUp
                | JobPhase::FinalizingExport
                | JobPhase::FinalizingPublication
        ) {
            return Ok(CancelOutcome::TooLate);
        }
        let mut wait = self
            .wait_state
            .lock()
            .map_err(|error| format!("native job wait mutex poisoned: {error}"))?;
        wait.official_publication_retry = None;
        if self.cancel_requested.swap(true, Ordering::AcqRel) {
            return Ok(CancelOutcome::AlreadyRequested);
        }
        status.state = JobState::Cancelling;
        drop(status);
        drop(wait);
        self.wait_changed.notify_all();
        Ok(CancelOutcome::Requested)
    }

    fn set_activation_permit(
        &self,
        permit: std::sync::Weak<Mutex<Option<admission::Permit>>>,
    ) -> Result<(), String> {
        let mut wait = self
            .wait_state
            .lock()
            .map_err(|_| "native job mutex poisoned".to_owned())?;
        if wait.activation_permit.is_some() || wait.restore_finalized {
            return Err("native activation permit is already assigned".into());
        }
        wait.activation_permit = Some(permit);
        Ok(())
    }

    fn request_finalize(
        &self,
        expected_revision: Option<i64>,
    ) -> Result<FinalizeOutcome, String> {
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
                | JobKind::RestorePortableBackup
                | JobKind::RestoreOfficialAccountSnapshot
                | JobKind::RestoreLegacyLocalBackup
                | JobKind::RestoreNativeSnapshot
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
        let mut wait = self
            .wait_state
            .lock()
            .map_err(|error| format!("native restore finalization mutex poisoned: {error}"))?;
        if wait.restore_finalized {
            return Ok(FinalizeOutcome::AlreadyRequested);
        }
        if let Some(slot) = &wait.activation_permit {
            let slot = slot.upgrade().ok_or_else(|| "native activation worker exited".to_owned())?;
            let mut owned = slot.lock().map_err(|_| "native activation permit is unavailable".to_owned())?;
            owned.as_mut().ok_or_else(|| "native activation permit was released".to_owned())?
                .upgrade_staging().map_err(|code| code.to_owned())?;
        }
        if expected_revision.is_some() {
            wait.activation_expected_revision = expected_revision;
        }
        wait.restore_finalized = true;
        status.state = JobState::Running;
        status.phase = JobPhase::ActivatingDatabase;
        status.detail = Some(activation_detail(
            status.detail.as_ref(),
            JobStage::Activating,
        ));
        drop(wait);
        drop(status);
        self.wait_changed.notify_all();
        Ok(FinalizeOutcome::Requested)
    }

    /// Revision the renderer last confirmed for this job's replacement, if it
    /// reported one while holding its fence.
    pub(crate) fn activation_expected_revision(&self) -> Result<Option<i64>, String> {
        Ok(self
            .wait_state
            .lock()
            .map_err(|error| format!("native restore finalization mutex poisoned: {error}"))?
            .activation_expected_revision)
    }

    pub(crate) fn wait_for_restore_finalization(&self) -> Result<Option<i64>, String> {
        {
            let mut status = self
                .status
                .lock()
                .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
            if !matches!(
                status.kind,
                JobKind::RestoreBlockRisuSave
                    | JobKind::RestorePortableBackup
                    | JobKind::RestoreOfficialAccountSnapshot
                    | JobKind::RestoreLegacyLocalBackup
                    | JobKind::RestoreNativeSnapshot
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
                status.detail = Some(activation_detail(
                    status.detail.as_ref(),
                    JobStage::Activating,
                ));
                return Ok(self.activation_expected_revision()?);
            }
            status.state = JobState::WaitingForInput;
            status.phase = JobPhase::AwaitingActivation;
            status.detail = Some(activation_detail(
                status.detail.as_ref(),
                JobStage::AwaitingActivation,
            ));
        }

        let mut wait = self
            .wait_state
            .lock()
            .map_err(|error| format!("native restore finalization mutex poisoned: {error}"))?;
        loop {
            if self.cancel_requested.load(Ordering::Acquire) {
                return Err("native restore was cancelled before activation".to_owned());
            }
            if wait.restore_finalized {
                return Ok(wait.activation_expected_revision);
            }
            wait = self
                .wait_changed
                .wait(wait)
                .map_err(|error| format!("native restore finalization mutex poisoned: {error}"))?;
        }
    }

    pub(crate) fn wait_for_official_publication_retry(
        &self,
        attempt: OfficialPublicationAttemptResult,
    ) -> Result<OfficialPublicationRetryInput, NativeJobError> {
        match &attempt {
            OfficialPublicationAttemptResult::ReauthenticationNeeded { status, .. }
                if *status == 403 => {}
            _ => {
                return Err(NativeJobError::new(
                    "store-error",
                    "official publication can only wait after an ordinary 403",
                ));
            }
        }
        let mut status = self.status.lock().map_err(|error| {
            NativeJobError::new(
                "store-error",
                format!("native job status mutex poisoned: {error}"),
            )
        })?;
        if self.is_cancel_requested() {
            return Err(publication_cancelled());
        }
        if status.kind != JobKind::OfficialPublicationUpload
            || status.state != JobState::Running
            || status.phase != JobPhase::UploadingDatabase
        {
            return Err(NativeJobError::new(
                "store-error",
                "official publication cannot wait from its current state",
            ));
        }
        let mut wait = self.wait_state.lock().map_err(|error| {
            NativeJobError::new(
                "store-error",
                format!("native publication wait mutex poisoned: {error}"),
            )
        })?;
        wait.official_publication_retry = None;
        status.state = JobState::WaitingForInput;
        status.phase = JobPhase::AwaitingPublicationRetry;
        status.publication_attempt = Some(attempt);
        drop(status);

        loop {
            if self.is_cancel_requested() {
                wait.official_publication_retry = None;
                return Err(publication_cancelled());
            }
            if let Some(retry) = wait.official_publication_retry.take() {
                return Ok(retry);
            }
            wait = self.wait_changed.wait(wait).map_err(|error| {
                NativeJobError::new(
                    "store-error",
                    format!("native publication wait mutex poisoned: {error}"),
                )
            })?;
        }
    }

    fn request_official_publication_retry(
        &self,
        request: OfficialPublicationRetryRequest,
    ) -> Result<(), String> {
        validate_official_publication_retry(&request)?;
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.kind != JobKind::OfficialPublicationUpload
            || status.state != JobState::WaitingForInput
            || status.phase != JobPhase::AwaitingPublicationRetry
            || self.is_cancel_requested()
        {
            return Err("official publication is not waiting for retry".to_owned());
        }
        let attempt_account = match status.publication_attempt.as_ref() {
            Some(OfficialPublicationAttemptResult::ReauthenticationNeeded {
                account_id,
                status,
                ..
            }) if *status == 403 => account_id,
            _ => return Err("official publication retry metadata is unavailable".to_owned()),
        };
        if attempt_account != &request.account_id {
            return Err("official publication retry account does not match".to_owned());
        }
        let mut wait = self
            .wait_state
            .lock()
            .map_err(|error| format!("native publication wait mutex poisoned: {error}"))?;
        if wait.official_publication_retry.is_some() {
            return Err("official publication retry is already occupied".to_owned());
        }
        wait.official_publication_retry = Some(OfficialPublicationRetryInput {
            session: request.session,
            save_date: request.save_date,
            credential: request.credential,
        });
        status.state = JobState::Running;
        status.phase = JobPhase::UploadingDatabase;
        status.publication_attempt = None;
        drop(wait);
        drop(status);
        self.wait_changed.notify_all();
        Ok(())
    }

    pub(crate) fn begin_official_publication_finalization(&self) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.kind != JobKind::OfficialPublicationUpload
            || !matches!(status.state, JobState::Running | JobState::Cancelling)
            || status.phase != JobPhase::UploadingDatabase
        {
            return Err("official publication cannot finalize from its current state".to_owned());
        }
        status.state = JobState::Running;
        status.phase = JobPhase::FinalizingPublication;
        reset_export_detail(&mut status);
        status.publication_attempt = None;
        Ok(())
    }

    pub(crate) fn start(&self, phase: JobPhase) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        let expected = match status.kind {
            JobKind::SnapshotBodies => JobPhase::CopyingMissingBodies,
            JobKind::RestorePortableBackup => JobPhase::ReadingSource,
            JobKind::ExportPortableBackup => JobPhase::WritingExport,
            JobKind::ExportRawRecovery => JobPhase::ReadingSource,
            JobKind::RestoreBlockRisuSave => JobPhase::ReadingSource,
            JobKind::RestoreOfficialAccountSnapshot => JobPhase::ReadingSource,
            JobKind::RestoreNativeSnapshot => JobPhase::ReadingSource,
            JobKind::RestoreLegacyLocalBackup => JobPhase::ReadingSource,
            JobKind::ExportBlockRisuSave => JobPhase::WritingExport,
            JobKind::ExportCompatibleLocalBackup => JobPhase::WritingExport,
            JobKind::ExportCharacterCharx => JobPhase::WritingExport,
            JobKind::ExportCharacterCard => JobPhase::WritingExport,
            JobKind::ExportRisuModule => JobPhase::WritingExport,
            JobKind::ExportDataset | JobKind::ExportGalleryFile | JobKind::ExportGalleryZip => JobPhase::WritingExport,
            JobKind::PrepareContentImport => JobPhase::ReadingSource,
            JobKind::ImportJpegAsset => JobPhase::ReadingSource,
            JobKind::KeiBackupUpload => JobPhase::WritingExport,
            JobKind::OfficialPublicationUpload => JobPhase::WritingExport,
            JobKind::RestoreOfficialAccountAssets => JobPhase::ReadingSource,
            JobKind::PublishOfficialAccountAssets => JobPhase::WritingExport,
        };
        if status.state != JobState::Queued || phase != expected {
            return Err("native job can only start from queued".to_owned());
        }
        status.state = JobState::Running;
        status.phase = phase;
        reset_export_detail(&mut status);
        Ok(())
    }

    pub(crate) fn set_export_progress(&self, progress: JobProgress, completed: u64, total: Option<u64>) -> Result<(), String> {
        self.set_progress(progress)?;
        self.set_export_bytes(completed, total)
    }

    pub(crate) fn set_export_bytes(&self, completed: u64, total: Option<u64>) -> Result<(), String> {
        let status = self.status();
        let stage = export_progress_stage(status.kind, status.phase)
            .ok_or_else(|| "native job is not exporting".to_owned())?;
        self.set_detail(JobDetail::new(stage, StageUnit::Bytes, completed, total, ImportCounts::default()))
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

    /// Records the import sub-stage, its own progress, and the running counts.
    /// Stages only move forward and counters only grow, so a stale or reordered
    /// report is rejected instead of making the dialog go backwards.
    pub(crate) fn set_detail(&self, detail: JobDetail) -> Result<(), String> {
        let detail = detail.bounded();
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.state != JobState::Running {
            return Err("native job detail requires a running job".to_owned());
        }
        validate_detail_transition(status.detail.as_ref(), &detail)?;
        status.detail = Some(detail);
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
        let direct_jpeg_activation = status.kind == JobKind::ImportJpegAsset
            && status.phase == JobPhase::ReadingSource
            && phase == JobPhase::ActivatingDatabase;
        if phase == JobPhase::Complete
            || (!direct_jpeg_activation && phase.rank() != status.phase.rank() + 1)
        {
            return Err("native job phase transition is invalid".to_owned());
        }
        status.phase = phase;
        reset_export_detail(&mut status);
        Ok(())
    }

    pub(crate) fn commit_export_publication(&self) -> Result<(), String> {
        let mut status = self
            .status
            .lock()
            .map_err(|error| format!("native job status mutex poisoned: {error}"))?;
        if status.phase != JobPhase::PublishingDestination
            || status.state != JobState::Running
            || self.cancel_requested.load(Ordering::Acquire)
        {
            return Err("native export publication cannot commit from its current state".to_owned());
        }
        status.phase = JobPhase::FinalizingExport;
        reset_export_detail(&mut status);
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
        status.publication_attempt = None;
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
                    item_kind: None,
                    owner: None,
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
                    item_kind: None,
                    owner: None,
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
                    item_kind: error.item_kind,
                    owner: error.owner,
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
        status.publication_attempt = None;
        drop(status);
        self.mark_terminal()
    }

    pub(crate) fn finish_success(&self, result: JobResultSummary) -> Result<(), String> {
        self.finish_success_with_receipt(result, None).map_err(|error| error.message)
    }

    fn finish_success_with_receipt(&self, mut result: JobResultSummary, receipt_root: Option<&Path>) -> Result<(), NativeJobError> {
        validate_result(&result).map_err(|error| NativeJobError::new("store-error", error))?;
        if result.publication.is_some() {
            return Err(NativeJobError::new("store-error", "non-publication job returned publication metadata"));
        }
        let context_warnings = self.status().warning_codes;
        for warning in context_warnings {
            if !result.warning_codes.contains(&warning) {
                result.warning_codes.push(warning);
            }
        }
        result.warning_codes.truncate(MAX_WARNING_CODES);
        validate_result(&result).map_err(|error| NativeJobError::new("store-error", error))?;
        let mut status = self
            .status
            .lock()
            .map_err(|error| NativeJobError::new("store-error", format!("native job status mutex poisoned: {error}")))?;
        if status.state != JobState::Running || self.is_cancel_requested() {
            return Err(NativeJobError::new("store-error", "native job cannot succeed from its current state"));
        }
        if status.kind == JobKind::ExportPortableBackup && result.handoff_path.is_some() {
            if let Some(root) = receipt_root {
                portable_export_receipts::persist(root, &status.job_id, &result)
                    .map_err(|error| NativeJobError::new("export-receipt-write-failed", error))?;
            }
        }
        status.state = JobState::Succeeded;
        status.phase = JobPhase::Complete;
        status.result = Some(result);
        status.error = None;
        status.prepared_content = None;
        status.publication_attempt = None;
        drop(status);
        self.mark_terminal().map_err(|error| NativeJobError::new("store-error", error))?;
        Ok(())
    }

    pub(crate) fn finish_official_publication_success(
        &self,
        mut result: JobResultSummary,
    ) -> Result<(), String> {
        validate_result(&result)?;
        if result.publication.is_none() {
            return Err("official publication result metadata is missing".to_owned());
        }
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
        if status.kind != JobKind::OfficialPublicationUpload
            || status.state != JobState::Running
            || status.phase != JobPhase::FinalizingPublication
        {
            return Err("official publication cannot succeed from its current state".to_owned());
        }
        status.state = JobState::Succeeded;
        status.phase = JobPhase::Complete;
        status.result = Some(result);
        status.error = None;
        status.prepared_content = None;
        status.publication_attempt = None;
        drop(status);
        self.mark_terminal()
    }

    pub(crate) fn finish_failure(&self, code: &str, message: &str) -> Result<(), String> {
        self.finish_failure_with_item(code, message, None, None)
    }

    fn finish_native_failure(&self, error: &NativeJobError) -> Result<(), String> {
        self.finish_failure_with_item(&error.code, &error.message, error.item_kind.as_deref(), error.owner.clone())
    }

    fn finish_failure_with_item(&self, code: &str, message: &str, item_kind: Option<&str>, owner: Option<FailureOwner>) -> Result<(), String> {
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
        crate::native_log::global_state().record(
            "error",
            "native-file-job",
            format!(
                "Native file job {} ({:?}) failed at {:?} [{code}]: {}",
                status.job_id,
                status.kind,
                status.phase,
                bounded_message(message),
            ),
        );
        status.state = JobState::Failed;
        status.phase = JobPhase::Complete;
        status.result = None;
        status.error = Some(JobFailure {
            code: code.to_owned(),
            message: bounded_message(message),
            item_kind: item_kind.map(str::to_owned),
            owner,
        });
        status.prepared_content = None;
        status.publication_attempt = None;
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
        // Plain data; recover from poisoning so prune keeps working after a
        // worker-thread panic.
        *self
            .terminal_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn validate_result(result: &JobResultSummary) -> Result<(), String> {
    validate_warning_codes(&result.warning_codes)?;
    if let Some(publication) = &result.publication {
        validate_official_publication_attempt(publication)?;
    }
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

fn validate_official_publication_retry(
    request: &OfficialPublicationRetryRequest,
) -> Result<(), String> {
    if request.job_id.is_empty() || request.job_id.len() > 128 {
        return Err("official publication retry job ID is invalid".to_owned());
    }
    validate_bounded_publication_string(&request.account_id, 512, false, "account")?;
    if let Some(session) = &request.session {
        validate_bounded_publication_string(session, 4096, true, "session")?;
    }
    validate_bounded_publication_string(&request.save_date, 128, false, "save date")?;
    validate_official_publication_credential(&request.credential)
}

fn validate_official_publication_credential(
    credential: &OfficialPublicationCredential,
) -> Result<(), String> {
    match credential {
        OfficialPublicationCredential::RisuAuth { token } => {
            validate_bounded_publication_string(token, 4096, false, "credential")
        }
    }
}

fn validate_official_publication_attempt(
    attempt: &OfficialPublicationAttemptResult,
) -> Result<(), String> {
    let (account_id, session, save_date, status) = match attempt {
        OfficialPublicationAttemptResult::Written {
            account_id,
            session,
            save_date,
            status,
            replacement_key,
            warning,
            ..
        } => {
            if !(200..300).contains(status) || *status == 304 {
                return Err("official publication written status is invalid".to_owned());
            }
            validate_bounded_publication_string(replacement_key, 4096, false, "replacement key")?;
            if let Some(warning) = warning {
                validate_bounded_publication_string(warning, 4096, true, "warning")?;
            }
            (account_id, session, save_date, status)
        }
        OfficialPublicationAttemptResult::NotModified {
            account_id,
            session,
            save_date,
            status,
            replacement_key,
        } => {
            if *status != 304 {
                return Err("official publication not-modified status is invalid".to_owned());
            }
            validate_bounded_publication_string(replacement_key, 4096, false, "replacement key")?;
            (account_id, session, save_date, status)
        }
        OfficialPublicationAttemptResult::AuthWarning {
            account_id,
            session,
            save_date,
            status,
            warning,
        }
        | OfficialPublicationAttemptResult::ReauthenticationNeeded {
            account_id,
            session,
            save_date,
            status,
            warning,
        } => {
            if *status != 403 {
                return Err("official publication authentication status is invalid".to_owned());
            }
            if let Some(warning) = warning {
                validate_bounded_publication_string(warning, 4096, true, "warning")?;
            }
            (account_id, session, save_date, status)
        }
    };
    validate_bounded_publication_string(account_id, 512, false, "account")?;
    if let Some(session) = session {
        validate_bounded_publication_string(session, 4096, true, "session")?;
    }
    validate_bounded_publication_string(save_date, 128, false, "save date")?;
    if !(100..=599).contains(status) {
        return Err("official publication HTTP status is invalid".to_owned());
    }
    Ok(())
}

fn validate_bounded_publication_string(
    value: &str,
    maximum_bytes: usize,
    allow_empty: bool,
    label: &str,
) -> Result<(), String> {
    if value.len() > maximum_bytes || (!allow_empty && value.is_empty()) {
        return Err(format!("official publication {label} is invalid"));
    }
    Ok(())
}

fn publication_cancelled() -> NativeJobError {
    NativeJobError::new("cancelled", "official publication was cancelled")
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
            Self::ReadingSource
            | Self::WritingExport
            | Self::AwaitingBackupSelection => 1,
            Self::AwaitingContentMapping
            | Self::StagingDatabase
            | Self::PublishingDestination
            | Self::UploadingDatabase => 2,
            Self::AwaitingActivation
            | Self::FinalizingExport
            | Self::AwaitingPublicationRetry
            | Self::FinalizingPublication => 3,
            Self::ActivatingDatabase => 4,
            Self::CopyingMissingBodies | Self::CleaningUp => 5,
            Self::Complete => 6,
        }
    }
}

mod export;
#[cfg(feature = "native-official-publication")]
mod publication;
pub(crate) mod restore;

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use serde_json::json;
    use std::fs;
    use std::io::{Cursor, Read, Write};
    use std::sync::Barrier;
    use std::thread;
    use std::time::Duration;
    use tempfile::TempDir;
    use zip::write::FileOptions;
    use zip::{CompressionMethod, ZipWriter};

    fn result(revision: i64) -> JobResultSummary {
        JobResultSummary {
            export_exclusions: None,
            revision,
            source_bytes: 1,
            source_sha256: "a".repeat(64),
            source_fingerprint_kind: crate::native_file_jobs::SourceFingerprintKind::WholeFileSha256,
            character_count: 0,
            preset_count: 0,
            warning_codes: Vec::new(),
            handoff_path: None,
            publication: None,
        }
    }

    #[test]
    fn jpeg_asset_job_request_keeps_source_and_explicit_destination_descriptor_only() {
        let request: NativeFileJobStartRequest = serde_json::from_value(json!({
            "kind": "import-jpeg-asset",
            "source": {
                "type": "androidSpool",
                "token": "spool-token"
            },
            "displayName": "portrait.jpeg",
            "destination": {
                "kind": "current-character-image",
                "characterId": "character-1"
            },
            "expectedRevision": 7
        }))
        .expect("decode JPEG asset request");

        match request {
            NativeFileJobStartRequest::ImportJpegAsset {
                source: JobSource::AndroidSpool { token },
                display_name,
                destination:
                    jpeg_asset::JpegAssetDestination::CurrentCharacterImage { character_id },
                expected_revision,
            } => {
                assert_eq!(token, "spool-token");
                assert_eq!(display_name, "portrait.jpeg");
                assert_eq!(character_id, "character-1");
                assert_eq!(expected_revision, 7);
            }
            _ => panic!("unexpected JPEG asset request"),
        }
    }

    #[test]
    fn retired_legacy_export_request_is_rejected() {
        assert!(serde_json::from_value::<NativeFileJobStartRequest>(json!({
            "kind": "export-legacy-local-backup", "expectedRevision": 8
        })).is_err());
    }

    #[test]
    fn portable_restore_reuses_the_existing_finalize_and_too_late_cancel_boundary() {
        let registry = JobRegistry::default();
        let job = registry
            .create_with_context(JobKind::RestorePortableBackup, Some(4), Vec::new())
            .unwrap();
        // A restore that has not activated reports no receipt at all, not a null one.
        let queued = serde_json::to_value(job.status()).unwrap();
        assert!(queued.get("activationRevision").is_none() && queued.get("activationAuthority").is_none());
        job.start(JobPhase::ReadingSource).unwrap();
        job.set_replaces_library(true).unwrap();
        job.set_phase(JobPhase::StagingDatabase).unwrap();

        let waiter = Arc::clone(&job);
        let waited = std::thread::spawn(move || waiter.wait_for_restore_finalization());
        let deadline = Instant::now() + Duration::from_secs(5);
        while job.status().state != JobState::WaitingForInput && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(job.status().state, JobState::WaitingForInput);
        assert_eq!(job.status().phase, JobPhase::AwaitingActivation);
        assert_eq!(
            job.request_finalize(None).unwrap(),
            FinalizeOutcome::Requested
        );
        assert_eq!(waited.join().unwrap(), Ok(None));
        assert_eq!(job.status().phase, JobPhase::ActivatingDatabase);
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::TooLate);
        job.finish_success(result(5)).unwrap();
        assert_eq!(job.status().replaces_library, Some(true));
        assert_eq!(serde_json::to_value(job.status()).unwrap()["replacesLibrary"], true);
        assert!(job.set_replaces_library(false).is_err());
    }

    fn publication_retry(account_id: &str, token: &str) -> OfficialPublicationRetryRequest {
        OfficialPublicationRetryRequest {
            job_id: String::new(),
            account_id: account_id.to_owned(),
            session: Some("fresh-session".to_owned()),
            save_date: "fresh-date".to_owned(),
            credential: OfficialPublicationCredential::RisuAuth {
                token: token.to_owned(),
            },
        }
    }

    fn reauthentication(account_id: &str) -> OfficialPublicationAttemptResult {
        OfficialPublicationAttemptResult::ReauthenticationNeeded {
            account_id: account_id.to_owned(),
            session: Some("stale-session".to_owned()),
            save_date: "stale-date".to_owned(),
            status: 403,
            warning: None,
        }
    }

    #[test]
    fn official_publication_retry_resumes_the_same_waiting_job_with_private_input() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::OfficialPublicationUpload).unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        job.set_phase(JobPhase::UploadingDatabase).unwrap();
        let worker = Arc::clone(&job);
        let waiting = thread::spawn(move || {
            worker.wait_for_official_publication_retry(reauthentication("account-1"))
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while job.status().state != JobState::WaitingForInput && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(job.status().state, JobState::WaitingForInput);
        assert_eq!(job.status().phase, JobPhase::AwaitingPublicationRetry);
        let mut retry = publication_retry("account-1", "fresh-secret");
        retry.job_id = job.id();
        assert_eq!(registry.retry_official_publication(retry), Ok(()));
        let resumed = waiting.join().unwrap().unwrap();
        assert_eq!(resumed.session.as_deref(), Some("fresh-session"));
        assert_eq!(
            resumed.credential,
            OfficialPublicationCredential::RisuAuth {
                token: "fresh-secret".to_owned()
            }
        );
        let status = job.status();
        assert_eq!(status.state, JobState::Running);
        assert_eq!(status.phase, JobPhase::UploadingDatabase);
        assert!(status.publication_attempt.is_none());
    }

    #[test]
    fn cancellation_clears_publication_retry_and_wakes_the_waiter_without_lost_wakeup() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::OfficialPublicationUpload).unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        job.set_phase(JobPhase::UploadingDatabase).unwrap();
        let worker = Arc::clone(&job);
        let waiting = thread::spawn(move || {
            worker.wait_for_official_publication_retry(reauthentication("account-1"))
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while job.status().state != JobState::WaitingForInput && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(job.status().state, JobState::WaitingForInput);
        assert_eq!(job.status().phase, JobPhase::AwaitingPublicationRetry);
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::Requested);
        let error = waiting.join().unwrap().expect_err("cancel waiting retry");
        assert_eq!(error.code, "cancelled");
    }

    #[test]
    fn terminal_publication_response_wins_a_racing_cancel_and_stays_durable() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::OfficialPublicationUpload).unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        job.set_phase(JobPhase::UploadingDatabase).unwrap();
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::Requested);
        job.begin_official_publication_finalization().unwrap();
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::TooLate);
        let mut summary = result(7);
        summary.publication = Some(OfficialPublicationAttemptResult::NotModified {
            account_id: "account-1".to_owned(),
            session: Some("session".to_owned()),
            save_date: "date".to_owned(),
            status: 304,
            replacement_key: "database/database.bin".to_owned(),
        });
        job.finish_official_publication_success(summary).unwrap();
        assert_eq!(job.status().state, JobState::Succeeded);
        assert!(job.status().result.unwrap().publication.is_some());
    }

    #[cfg(feature = "native-official-publication")]
    fn sealed_publication_job(repository_root: &Path, job_id: &str) -> (String, PathBuf) {
        use crate::asset_repository::job_pins::{CasJobKind, CasObjectRole, DurableCasJob};

        let mut store = crate::persistent_store::PersistentStore::open(repository_root).unwrap();
        let cas = crate::asset_repository::PayloadCas::new(repository_root).unwrap();
        let payload = cas.prepare_bytes(b"publication-root").unwrap();
        let mut durable = DurableCasJob::begin(
            repository_root,
            job_id,
            CasJobKind::OfficialPublicationOrExportPreparation,
            crate::asset_repository::job_pins::CasJobOwner::for_test(),
            1,
        )
        .unwrap();
        durable
            .pin_existing(
                &cas,
                &payload.content_hash,
                payload.byte_size,
                CasObjectRole::DirectObject,
            )
            .unwrap();
        durable.seal(&mut store, 2).unwrap();
        (payload.content_hash, durable.journal_path().to_owned())
    }

    #[cfg(feature = "native-official-publication")]
    fn finish_written_publication(job: &JobControl) {
        job.start(JobPhase::WritingExport).unwrap();
        job.set_phase(JobPhase::UploadingDatabase).unwrap();
        job.begin_official_publication_finalization().unwrap();
        let mut summary = result(1);
        summary.publication = Some(OfficialPublicationAttemptResult::Written {
            account_id: "account-1".to_owned(),
            session: Some("session".to_owned()),
            save_date: "date".to_owned(),
            status: 200,
            replacement_key: "database/database.bin".to_owned(),
            warning: None,
            reload_session: false,
        });
        job.finish_official_publication_success(summary).unwrap();
    }

    #[cfg(feature = "native-official-publication")]
    #[test]
    fn publication_receipt_ack_releases_committed_roots_only_when_forgotten() {
        use crate::asset_repository::job_pins::collect_durable_cas_job_roots;

        let directory = TempDir::new().unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let job = state
            .registry
            .create(JobKind::OfficialPublicationUpload)
            .unwrap();
        let (hash, _) = sealed_publication_job(directory.path(), &job.id());
        finish_written_publication(&job);

        assert!(collect_durable_cas_job_roots(directory.path())
            .object_hashes
            .contains(&hash));
        assert_eq!(job.status().state, JobState::Succeeded);

        assert!(state.forget(&job.id()).unwrap());
        assert!(collect_durable_cas_job_roots(directory.path())
            .object_hashes
            .is_empty());
    }

    #[cfg(feature = "native-official-publication")]
    #[test]
    fn terminal_publication_receipt_is_not_pruned_before_acknowledgement() {
        let registry = JobRegistry::with_retention(0, Duration::ZERO);
        let publication = registry.create(JobKind::OfficialPublicationUpload).unwrap();
        let publication_id = publication.id();
        finish_written_publication(&publication);

        let ordinary = registry.create(JobKind::ExportBlockRisuSave).unwrap();
        ordinary.start(JobPhase::WritingExport).unwrap();
        ordinary.finish_success(result(1)).unwrap();

        assert_eq!(
            registry.status(&publication_id).unwrap().state,
            JobState::Succeeded
        );
    }

    #[cfg(feature = "native-official-publication")]
    #[test]
    fn terminal_publication_failure_forget_releases_aborted_roots() {
        use crate::asset_repository::job_pins::collect_durable_cas_job_roots;

        let directory = TempDir::new().unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let job = state
            .registry
            .create(JobKind::OfficialPublicationUpload)
            .unwrap();
        let (hash, _) = sealed_publication_job(directory.path(), &job.id());
        job.start(JobPhase::WritingExport).unwrap();
        job.finish_failure("transport-failed", "offline").unwrap();
        assert!(collect_durable_cas_job_roots(directory.path())
            .object_hashes
            .contains(&hash));

        assert!(state.forget(&job.id()).unwrap());
        assert!(collect_durable_cas_job_roots(directory.path())
            .object_hashes
            .is_empty());
    }

    #[cfg(feature = "native-official-publication")]
    #[test]
    fn publication_release_error_retains_terminal_receipt_and_journal() {
        let directory = TempDir::new().unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let job = state
            .registry
            .create(JobKind::OfficialPublicationUpload)
            .unwrap();
        let (_, journal) = sealed_publication_job(directory.path(), &job.id());
        finish_written_publication(&job);
        fs::OpenOptions::new()
            .append(true)
            .open(&journal)
            .unwrap()
            .write_all(b"not-json\n")
            .unwrap();

        assert!(state.forget(&job.id()).is_err());
        assert_eq!(state.status(&job.id()).unwrap().state, JobState::Succeeded);
        assert!(journal.is_file());
    }

    #[cfg(feature = "native-official-publication")]
    #[test]
    fn process_loss_keeps_unacknowledged_publication_roots_as_startup_blockers() {
        use crate::asset_repository::job_pins::collect_durable_cas_job_roots;

        let directory = TempDir::new().unwrap();
        let native_root = directory.path().join("native-file-jobs");
        let state = NativeFileJobState::initialize(native_root.clone());
        let job = state
            .registry
            .create(JobKind::OfficialPublicationUpload)
            .unwrap();
        let (hash, _) = sealed_publication_job(directory.path(), &job.id());
        finish_written_publication(&job);
        drop(job);
        drop(state);

        let _restarted = NativeFileJobState::initialize(native_root);
        assert!(collect_durable_cas_job_roots(directory.path())
            .object_hashes
            .contains(&hash));
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

    fn risum_fixture(module: Value, assets: &[&[u8]]) -> Vec<u8> {
        let map = include_bytes!("../../../src/ts/rpack/rpack_map.bin");
        let encode = |bytes: &[u8]| {
            bytes
                .iter()
                .map(|byte| map[*byte as usize])
                .collect::<Vec<_>>()
        };
        let metadata = encode(
            serde_json::to_string(&json!({ "type": "risuModule", "module": module }))
                .unwrap()
                .as_bytes(),
        );
        let mut bytes = vec![111, 0];
        bytes.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&metadata);
        for asset in assets {
            let encoded = encode(asset);
            bytes.push(1);
            bytes.extend_from_slice(&(encoded.len() as u32).to_le_bytes());
            bytes.extend_from_slice(&encoded);
        }
        bytes.push(0);
        bytes
    }

    fn wait_for_content_job(state: &NativeFileJobState, job_id: &str) -> JobStatus {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let status = state.status(job_id).unwrap();
            if status.state.is_terminal() {
                return status;
            }
            assert!(Instant::now() < deadline, "content preparation timed out: {status:?}");
            thread::sleep(Duration::from_millis(5));
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

        assert_eq!(error.code, "payload-too-large");
        assert_eq!(error.item_kind.as_deref(), Some("card"));
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
                "{{\"token\":\"{token}\",\"state\":\"ready\",\"displayName\":\"database.risudat\",\"bytes\":9,\"totalBytes\":12,\"importDestination\":null}}"
            ),
            1,
        );
        let jobs_root = directory.path().join("jobs");
        fs::create_dir_all(&jobs_root).unwrap();
        let job_id = Uuid::new_v4().to_string();
        let owned = create_owned_directory(&jobs_root, &job_id).unwrap();

        let source = claim_spool_source(directory.path(), &token, &owned).unwrap();

        assert_eq!(source.path.file_name().unwrap(), "source.risudat");
        assert!(source.path.starts_with(owned.canonicalize().unwrap()));
        assert_eq!(source.opened.total_bytes, 9);
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
                Ok(mut source) => {
                    let mut bytes = Vec::new();
                    source.opened.file.read_to_end(&mut bytes).unwrap();
                    assert_eq!(bytes, b"RISUSAVE\0");
                }
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
    fn portable_adoption_retains_exclusion_until_exact_idempotent_receipt() {
        let registry = JobRegistry::default();
        let admission = Arc::new(admission::Admission::default());
        let permit = Arc::new(Mutex::new(Some(admission.file(true).unwrap())));
        let job = registry.create(JobKind::RestorePortableBackup).unwrap();
        job.set_activation_permit(Arc::downgrade(&permit)).unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        job.set_device_session("synthetic-device-session").unwrap();
        job.publish_portable_activation(7, "3".into()).unwrap();
        assert!(admission.server().is_err());
        assert!(job.confirm_portable_adoption("07", "3", "synthetic-device-session", ||Ok(())).is_err());
        assert!(job.confirm_portable_adoption("8", "3", "synthetic-device-session", ||Ok(())).is_err());
        assert!(job.confirm_portable_adoption("7", "4", "synthetic-device-session", ||Ok(())).is_err());
        assert!(job.confirm_portable_adoption("7", "3", "other-session", ||Ok(())).is_err());
        assert!(job.confirm_portable_adoption("7", "3", "synthetic-device-session", ||Err("Device recovery has not completed".into())).is_err());
        assert!(admission.server().is_err());
        let worker = Arc::clone(&job);
        let (done, completed) = std::sync::mpsc::channel();
        let joined = std::thread::spawn(move || {
            worker.wait_for_portable_adoption().unwrap();
            done.send(()).unwrap();
        });
        assert!(completed.try_recv().is_err());
        job.confirm_portable_adoption("7", "3", "synthetic-device-session", ||Ok(())).unwrap();
        completed.recv().unwrap();
        joined.join().unwrap();
        assert!(admission.server().is_ok());
        job.confirm_portable_adoption("7", "3", "synthetic-device-session", ||panic!("matching completed ACK must be idempotent after session cleanup")).unwrap();
        assert!(job.confirm_portable_adoption("9", "3", "synthetic-device-session", ||Ok(())).is_err());
    }

    #[test]
    fn portable_adoption_fixture_keeps_mandatory_finalization_and_actual_exclusion() {
        let root=tempfile::tempdir().unwrap();
        let state=NativeFileJobState::initialize(root.path().join("native-file-jobs"));
        let (job,slot)=state.create_portable_restore_fixture(7).unwrap();
        assert!(job.requires_restore_finalization);
        assert_eq!(job.status().expected_revision,Some(7));
        assert!(state.admission.server().is_err());
        assert!(slot.lock().unwrap().is_some());
        drop(slot);
        assert!(state.admission.server().is_ok());
    }

    #[test]
    fn upstream_finalize_upgrades_owned_staging_and_retains_exclusion_after_lost_response() {
        let directory = TempDir::new().unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let registry = Arc::clone(&state.registry);
        let admission = Arc::clone(&state.admission);
        let permit = Arc::new(Mutex::new(Some(admission.staging().unwrap())));
        let job = registry
            .create_with_context(JobKind::RestoreBlockRisuSave, Some(3), Vec::new())
            .unwrap();
        job.set_activation_permit(Arc::downgrade(&permit)).unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        job.set_phase(JobPhase::StagingDatabase).unwrap();
        let waiter = Arc::clone(&job);
        let waited = std::thread::spawn(move || waiter.wait_for_restore_finalization());
        while job.status().state != JobState::WaitingForInput {
            std::thread::yield_now();
        }
        let receive = admission.server().unwrap();
        assert_eq!(state.finalize(&job.id(), Some(9)).unwrap_err().code, "library-operation-busy");
        assert_eq!(state.finalize("missing-synthetic-job", Some(9)).unwrap(), FinalizeOutcome::Missing);
        assert_eq!(job.status().state, JobState::WaitingForInput);
        assert_eq!(job.activation_expected_revision().unwrap(), None);
        drop(receive);
        assert_eq!(
            registry.finalize(&job.id(), Some(9)).unwrap(),
            FinalizeOutcome::Requested
        );
        // Losing the renderer's response neither releases admission nor activates twice.
        assert!(admission.server().is_err());
        assert_eq!(
            registry.finalize(&job.id(), Some(99)).unwrap(),
            FinalizeOutcome::AlreadyRequested
        );
        assert_eq!(waited.join().unwrap(), Ok(Some(9)));
        assert_eq!(registry.cancel(&job.id()).unwrap(), CancelOutcome::TooLate);
        assert!(admission.server().is_err());
        job.finish_success(result(10)).unwrap();
        drop(permit);
        assert!(admission.server().is_ok());
        assert_eq!(
            registry.finalize(&job.id(), Some(99)).unwrap(),
            FinalizeOutcome::Terminal
        );
    }

    #[test]
    fn upstream_terminal_receipt_follows_cleanup_and_released_admission() {
        let registry = Arc::new(JobRegistry::default());
        let admission = Arc::new(admission::Admission::default());
        let permit = Arc::new(Mutex::new(Some(admission.staging().unwrap())));
        permit
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .upgrade_staging()
            .unwrap();
        let job = registry
            .create_with_context(JobKind::RestoreBlockRisuSave, Some(3), Vec::new())
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        job.set_phase(JobPhase::StagingDatabase).unwrap();
        let (cleanup_done, allow_cleanup) = std::sync::mpsc::channel();
        let (released, admission_released) = std::sync::mpsc::channel();
        let (publish, allow_publish) = std::sync::mpsc::channel();
        let worker_job = Arc::clone(&job);
        let worker = std::thread::spawn(move || {
            allow_cleanup.recv().unwrap();
            release_worker_admission(&permit);
            released.send(()).unwrap();
            allow_publish.recv().unwrap();
            finish_worker_outcome(
                &worker_job,
                JobKind::RestoreBlockRisuSave,
                Ok(result(4)),
                Vec::new(),
                None,
            );
        });
        assert!(admission.server().is_err());
        assert_eq!(job.status().state, JobState::Running);
        cleanup_done.send(()).unwrap();
        admission_released.recv().unwrap();
        assert_eq!(job.status().state, JobState::Running);
        assert!(admission.server().is_ok());
        publish.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(job.status().state, JobState::Succeeded);
        assert!(admission.server().is_ok());
    }

    #[test]
    fn upstream_cancelled_staging_releases_worker_permit_without_finalizing() {
        let registry = Arc::new(JobRegistry::default());
        let admission = Arc::new(admission::Admission::default());
        let permit = Arc::new(Mutex::new(Some(admission.staging().unwrap())));
        let job = registry
            .create_with_context(JobKind::RestoreBlockRisuSave, Some(3), Vec::new())
            .unwrap();
        job.set_activation_permit(Arc::downgrade(&permit)).unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        job.set_phase(JobPhase::StagingDatabase).unwrap();
        let waiter = Arc::clone(&job);
        let waited = std::thread::spawn(move || waiter.wait_for_restore_finalization());
        while job.status().state != JobState::WaitingForInput {
            std::thread::yield_now();
        }
        assert_eq!(
            registry.cancel(&job.id()).unwrap(),
            CancelOutcome::Requested
        );
        assert!(waited.join().unwrap().is_err());
        job.finish_failure("cancelled", "Synthetic cancellation")
            .unwrap();
        drop(permit);
        assert!(admission.file(true).is_ok());
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
            registry.finalize(&job.id(), Some(9)).unwrap(),
            FinalizeOutcome::Requested
        );
        assert_eq!(waited.join().unwrap(), Ok(Some(9)));
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
    fn content_prepare_risum_preserves_occurrences_and_prepares_native_owner_roots() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("module.RISUM");
        fs::write(
            &source,
            risum_fixture(
                json!({
                    "id": "source-id",
                    "name": "Native module",
                    "unknownFutureField": { "retained": true },
                    "assets": [
                        ["duplicate", "legacy-a", ".unsafe/path", { "future": true }],
                        ["duplicate", "legacy-b", "bin"],
                        ["undeclared", "legacy-c", null]
                    ]
                }),
                &[b"exact ordinary bytes", b"", b"RIFF\x04\0\0\0WEBPVP8 "],
            ),
        )
        .unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = start_test_content_job(&state, &source, "module.RISUM");

        let prepared = wait_for_content_job(&state, &started.job_id);

        assert_eq!(prepared.state, JobState::Succeeded);
        let content = prepared.prepared_content.as_ref().expect("prepared RISUM");
        let serialized = serde_json::to_value(content).unwrap();
        assert_eq!(
            serialized.pointer("/format").and_then(Value::as_str),
            Some("risu-module")
        );
        assert_eq!(
            serialized.pointer("/metadata/id"),
            Some(&json!("source-id"))
        );
        assert_eq!(
            serialized.pointer("/metadata/unknownFutureField/retained"),
            Some(&json!(true))
        );
        assert_eq!(serialized.pointer("/assets/0/position"), Some(&json!(0)));
        assert_eq!(serialized.pointer("/assets/1/position"), Some(&json!(1)));
        assert_eq!(
            serialized.pointer("/assets/0/ext"),
            Some(&json!(".unsafe/path"))
        );
        let expected_logical_id = format!(
            "assets/{}.bin",
            serialized
                .pointer("/assets/0/objectHash")
                .and_then(Value::as_str)
                .unwrap()
        );
        assert_eq!(
            serialized
                .pointer("/assets/0/logicalId")
                .and_then(Value::as_str),
            Some(expected_logical_id.as_str())
        );
        assert_eq!(serialized.pointer("/assets/1/ext"), Some(&json!("bin")));
        assert_eq!(serialized.pointer("/assets/2/ext"), Some(&json!("webp")));
        assert_eq!(
            serialized
                .pointer("/assets/2/logicalId")
                .and_then(Value::as_str),
            Some(
                format!(
                    "assets/{}.webp",
                    serialized
                        .pointer("/assets/2/objectHash")
                        .and_then(Value::as_str)
                        .unwrap()
                )
                .as_str()
            )
        );
        assert_eq!(serialized.pointer("/metadata/assets/2/2"), Some(&json!("")));
        assert_eq!(
            serialized.pointer("/metadata/assets/0/3/future"),
            Some(&json!(true))
        );
        assert!(serialized.pointer("/assets/0/token").is_none());
        assert!(serialized.pointer("/assets/0/referenceKey").is_none());
        assert_eq!(serialized.pointer("/ownerHead/present"), Some(&json!(true)));
        assert_eq!(serialized.pointer("/ownerHead/entryCount"), Some(&json!(3)));
        let manifest = crate::asset_repository::owner_manifest_codec::decode_owner_manifest(
            &crate::asset_repository::PayloadCas::new(directory.path())
                .unwrap()
                .read_object(
                    serialized
                        .pointer("/ownerHead/manifestHash")
                        .and_then(Value::as_str)
                        .unwrap(),
                )
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        // TypeScript activation keeps each metadata tuple with its logical ID
        // and requires the owner manifest to carry exactly that tuple.
        for (position, entry) in manifest.iter().enumerate() {
            let tuple = &serialized["metadata"]["assets"][position];
            assert_eq!(
                json!(entry.tuple),
                json!([
                    tuple[0],
                    serialized["assets"][position]["logicalId"],
                    tuple[2]
                ])
            );
        }
        assert_eq!(content.cas_session_id, started.job_id);
        let encoded = serde_json::to_string(content).unwrap();
        assert!(!encoded.contains("stagedPath"));
        assert!(!encoded.contains("legacy-a"));
        assert!(!encoded.contains("exact ordinary bytes"));

        let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
        assert_eq!(
            cas.read_object(
                serialized
                    .pointer("/assets/0/objectHash")
                    .and_then(Value::as_str)
                    .unwrap()
            )
            .unwrap(),
            Some(b"exact ordinary bytes".to_vec())
        );
        assert_eq!(
            cas.read_object(
                serialized
                    .pointer("/assets/1/objectHash")
                    .and_then(Value::as_str)
                    .unwrap()
            )
            .unwrap(),
            Some(Vec::new())
        );
        let session = crate::asset_repository::job_pins::DurableCasJob::open(
            directory.path(),
            &started.job_id,
        )
        .unwrap();
        assert_eq!(session.pin_count(), 4);
        assert!(!session.is_sealed());
    }

    #[test]
    fn prepared_asset_rich_modules_commit_units_and_reopen_for_bin_and_risum() {
        use crate::persistent_store::{AssetAlias, AssetOwnerHead, PersistentStore, WorkingSetCommit};

        for name in ["module.bin", "module.RISUM"] {
            let directory = TempDir::new().unwrap();
            let mut store = PersistentStore::open(directory.path()).unwrap();
            let payloads: Vec<Vec<u8>> = (0..300).map(|index| format!("synthetic-payload-{index}").into_bytes()).collect();
            let bytes: Vec<&[u8]> = payloads.iter().map(Vec::as_slice).collect();
            let tuples: Vec<Value> = (0..payloads.len()).map(|index| json!([format!("asset-{index}"), "source-key", "bin"])).collect();
            let source = directory.path().join(name);
            fs::write(&source, risum_fixture(json!({"id":"source-id", "name":"Synthetic module", "assets":tuples}), &bytes)).unwrap();
            let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
            let started = start_test_content_job(&state, &source, name);
            let prepared = wait_for_content_job(&state, &started.job_id);
            let content = serde_json::to_value(prepared.prepared_content.as_ref().expect("prepared module")).unwrap();
            assert_eq!(content["format"], "risu-module");
            let mut module = content["metadata"].clone();
            module["id"] = json!("imported-module");
            let aliases: Vec<AssetAlias> = content["assets"].as_array().unwrap().iter().enumerate().map(|(index, asset)| {
                module["assets"][index][1] = asset["logicalId"].clone();
                serde_json::from_value(json!({"kind":"asset", "key":asset["logicalId"], "objectHash":asset["objectHash"], "size":asset["byteSize"], "mime":asset["mime"], "name":asset["name"], "ext":asset["ext"]})).unwrap()
            }).collect();
            let mut head = content["ownerHead"].clone();
            head["owner"] = json!({"kind":"root-module-assets", "moduleId":"imported-module"});
            let head: AssetOwnerHead = serde_json::from_value(head).unwrap();
            let mut session = crate::asset_repository::job_pins::DurableCasJob::open(directory.path(), &started.job_id).unwrap();
            session.seal(&mut store, 2).unwrap();
            let input: WorkingSetCommit = serde_json::from_value(json!({
                "expectedRevision":store.revision().unwrap(),
                "unitMutations":[
                    {"key":"[\"exists\",\"modules\",\"imported-module\"]", "type":"set", "value":true},
                    {"key":"[\"record\",\"modules\",\"imported-module\"]", "type":"set", "value":module}
                ],
                "assetOwnerHeads":[head]
            })).unwrap();
            store.commit_with_asset_aliases(&input, &aliases).unwrap();
            drop(store);
            let reopened = PersistentStore::open(directory.path()).unwrap();
            assert_eq!(reopened.read_asset_owner_head(&head.owner, None).unwrap().unwrap().value, head);
            assert_eq!(reopened.materialize(None).unwrap()["modules"][0], module);
            let cas = crate::asset_repository::PayloadCas::new(directory.path()).unwrap();
            for (alias, payload) in aliases.iter().zip(payloads) {
                assert_eq!(reopened.read_asset_alias("asset", &alias.key, None).unwrap().unwrap().value, *alias);
                assert_eq!(cas.read_object(alias.object_hash.as_ref().unwrap()).unwrap(), Some(payload));
            }
        }
    }

    #[test]
    fn content_prepare_risum_distinguishes_absent_and_present_empty_assets() {
        for (name, module, expected_present, expected_pins) in [
            ("absent.risum", json!({ "name": "Absent" }), false, 0),
            (
                "empty.risum",
                json!({ "name": "Empty", "assets": [] }),
                true,
                1,
            ),
        ] {
            let directory = TempDir::new().unwrap();
            let source = directory.path().join(name);
            fs::write(&source, risum_fixture(module, &[])).unwrap();
            let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
            let started = start_test_content_job(&state, &source, name);
            let prepared = wait_for_content_job(&state, &started.job_id);
            let content = prepared.prepared_content.as_ref().expect("prepared RISUM");
            let serialized = serde_json::to_value(content).unwrap();
            assert_eq!(
                serialized.pointer("/ownerHead/present"),
                Some(&json!(expected_present))
            );
            assert_eq!(
                serialized.pointer("/metadata/assets").is_some(),
                expected_present
            );
            let session = crate::asset_repository::job_pins::DurableCasJob::open(
                directory.path(),
                &started.job_id,
            )
            .unwrap();
            assert_eq!(session.pin_count(), expected_pins);
            assert!(!session.is_sealed());
        }
    }

    #[test]
    fn risum_import_limits_support_large_module_libraries() {
        let limits = super::content::risum_import_limits();
        assert_eq!(limits.max_payload_count, 50_000);
        assert_eq!(limits.max_aggregate_payload_bytes, 10 * 1024 * 1024 * 1024);
    }

    #[test]
    #[ignore = "synthetic large content import measurements"]
    fn large_content_import_benchmark() {
        for (count, size) in [(1000, 100 * 1024), (2500, 200 * 1024), (10000, 1024)] {
            let directory = TempDir::new().unwrap();
            let source = directory.path().join("synthetic.charx");
            let mut archive = ZipWriter::new(fs::File::create(&source).unwrap());
            let options = FileOptions::default().compression_method(CompressionMethod::Stored);
            let references = (0..count).map(|i| json!({"type":"x-risu-asset", "uri":format!("embeded://assets/{i}.bin"), "name":format!("asset-{i}"), "ext":"bin"})).collect::<Vec<_>>();
            archive.start_file("card.json", options).unwrap();
            archive.write_all(&serde_json::to_vec(&json!({"spec":"chara_card_v3","spec_version":"3.0","data":{"name":"synthetic","extensions":{},"assets":references}})).unwrap()).unwrap();
            for i in 0..count {
                let mut bytes = vec![42_u8; size];
                bytes[..8].copy_from_slice(&(i as u64).to_le_bytes());
                archive
                    .start_file(format!("assets/{i}.bin"), options)
                    .unwrap();
                archive.write_all(&bytes).unwrap();
            }
            archive.finish().unwrap();
            let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
            let now = Instant::now();
            let started = start_test_content_job(&state, &source, "synthetic.charx");
            let status = loop {
                let status = state.status(&started.job_id).unwrap();
                if status.state.is_terminal() {
                    break status;
                }
                assert!(
                    now.elapsed() < Duration::from_secs(300),
                    "import benchmark timed out"
                );
                thread::sleep(Duration::from_millis(100));
            };
            let elapsed = now.elapsed();
            assert_eq!(status.state, JobState::Succeeded, "{:?}", status.error);
            let content = status.prepared_content.unwrap();
            assert_eq!(content.assets.len(), count);
            assert_eq!(
                status.detail.unwrap().counts.attachments_prepared,
                count as u64
            );
            assert_eq!(
                content.assets.iter().map(|a| a.byte_size).sum::<u64>(),
                (count * size) as u64
            );
            assert!(!state
                .root
                .join("jobs")
                .join(&started.job_id)
                .join("source.charx")
                .exists());
            println!(
                "native-content count={count} bytes={} elapsed_ms={}",
                count * size,
                elapsed.as_millis()
            );
        }
    }

    #[test]
    fn charx_overlay_accepts_ten_thousand_lore_entries_above_eight_mib() {
        let directory = TempDir::new().unwrap();
        let source = directory.path().join("large-lore.charx");
        let lore = vec![json!({"key": "synthetic", "content": "x".repeat(1024)}); 10_000];
        let module = risum_fixture(
            json!({"name": "synthetic", "id": "module", "lorebook": lore, "assets": []}),
            &[],
        );
        assert!(module.len() > 8 * 1024 * 1024);
        let mut archive = ZipWriter::new(fs::File::create(&source).unwrap());
        let options = FileOptions::default().compression_method(CompressionMethod::Stored);
        archive.start_file("card.json", options).unwrap();
        archive.write_all(br#"{"spec":"chara_card_v3","spec_version":"3.0","data":{"name":"synthetic","extensions":{},"assets":[]}}"#).unwrap();
        archive.start_file("module.risum", options).unwrap();
        archive.write_all(&module).unwrap();
        archive.finish().unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let started = start_test_content_job(&state, &source, "large-lore.charx");
        let status = wait_for_content_job(&state, &started.job_id);
        assert_eq!(status.state, JobState::Succeeded, "{:?}", status.error);
        let content = status.prepared_content.unwrap();
        assert_eq!(content.module.unwrap().lorebook.unwrap().len(), 10_000);
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
    fn jpeg_asset_job_rejects_and_cleans_a_spool_with_a_different_display_name() {
        let directory = TempDir::new().unwrap();
        let job_root = directory.path().join("native-file-jobs");
        let state = NativeFileJobState::initialize(job_root.clone());
        let token = Uuid::new_v4().to_string();
        write_literal_spool(
            &job_root,
            &token,
            &format!(
                "{{\"token\":\"{token}\",\"state\":\"ready\",\"displayName\":\"portrait.jpg\",\"bytes\":9,\"totalBytes\":null}}"
            ),
            1,
        );

        let error = state
            .spawn(
                NativeFileJobTask::ImportJpegAsset {
                    opened_source: None,
                    source: JobSource::AndroidSpool {
                        token: token.clone(),
                    },
                    display_name: "portrait.jpeg".to_owned(),
                    destination: jpeg_asset::JpegAssetDestination::CurrentCharacterImage {
                        character_id: "character-1".to_owned(),
                    },
                    expected_revision: 0,
                    store: crate::persistent_store::PersistentStore::open(directory.path())
                        .unwrap(),
                },
                false,
            )
            .expect_err("reject mismatched JPEG spool display name");

        assert_eq!(error.code, "invalid-source");
        assert!(!job_root.join("sources").join(token).exists());
        assert_eq!(fs::read_dir(job_root.join("jobs")).unwrap().count(), 0);
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
        assert!(!directory.path().join("assets").exists());
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
        assert!(directory.path().join("assets/job-pins").is_dir());
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
                .join("assets/objects")
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
            crate::asset_repository::job_pins::CasJobOwner::for_test(),
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
            owner_head: None,
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
        let description = "synthetic description";
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
        let (entered, preparation_started) = std::sync::mpsc::sync_channel(1);
        let (resume_preparation, resume) = std::sync::mpsc::channel();
        *state.registry.content_preparation_gate.lock().unwrap() = Some(Arc::new(ContentPreparationGate {
            entered,
            resume: Mutex::new(resume),
        }));
        let started = state
            .start_content_for_test(NativeFileJobStartRequest::PrepareContentImport {
                source: JobSource::DesktopPath {
                    path: source.to_string_lossy().into_owned(),
                },
                display_name: "large-card.json".to_owned(),
            })
            .expect("start content preparation");

        let deadline = Instant::now() + Duration::from_secs(5);
        preparation_started.recv_timeout(Duration::from_secs(5)).expect("content preparation did not start");
        assert_eq!(
            state.cancel(&started.job_id).unwrap(),
            CancelOutcome::Requested
        );
        resume_preparation.send(()).unwrap();
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
        let wait_guard = job.wait_state.lock().unwrap();
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
    fn portable_export_requires_an_active_revision() {
        let selection = portable::PortableSelection::default();
        assert!(!portable_export_requires_revision(&None, Some(7), &selection).unwrap());
        assert!(portable_export_requires_revision(&None, None, &selection).is_err());
        assert!(portable_export_requires_revision(&Some(JobSource::DesktopPath { path: "synthetic".into() }), Some(7), &selection).is_err());
    }


    #[test]
    fn native_content_export_requests_are_descriptor_only_and_recovery_kinds_stay_exact() {
        let plain_shape = json!({
            "kind": "export-character-charx",
            "destination": null,
            "expectedRevision": 7,
            "characterId": "character-7",
            "card": {"spec": "chara_card_v3"},
            "module": {}
        });
        let plain: NativeFileJobStartRequest = serde_json::from_value(plain_shape).unwrap();
        assert!(matches!(
            plain,
            NativeFileJobStartRequest::ExportCharacterCharx {
                portrait_rendition: None,
                container: CharacterCharxContainer::PlainCharx,
                ..
            }
        ));

        let cases = [
            json!({
                "kind": "export-character-charx",
                "container": "appended-charx-jpeg",
                "destination": null,
                "expectedRevision": 8,
                "characterId": "character-8"
            }),
            json!({
                "kind": "export-character-card",
                "format": "json-card",
                "destination": null,
                "expectedRevision": 9,
                "characterId": "character-9",
                "metadata": {"spec": "chara_card_v3", "spec_version": "3.0"}
            }),
            json!({
                "kind": "export-character-card",
                "format": "png-card",
                "destination": "C:\\chosen\\character.png",
                "expectedRevision": 10,
                "characterId": "character-10",
                "metadata": {"spec": "chara_card_v3", "spec_version": "3.0"}
            }),
            json!({
                "kind": "export-risu-module",
                "destination": null,
                "expectedRevision": 11,
                "moduleIndex": 3
            }),
            json!({
                "kind": "export-dataset",
                "destination": "C:\\chosen\\dataset.json",
                "expectedRevision": 12
            }),
        ];
        for case in &cases {
            assert!(case.get("assets").is_none());
            assert!(case.get("payload").is_none());
            assert!(case.get("module").is_none());
            assert!(case.get("card").is_none());
        }
        let appended: NativeFileJobStartRequest = serde_json::from_value(cases[0].clone()).unwrap();
        assert!(matches!(
            appended,
            NativeFileJobStartRequest::ExportCharacterCharx {
                portrait_rendition: None,
                destination: None,
                expected_revision: 8,
                character_id,
                container: CharacterCharxContainer::AppendedCharxJpeg,
                card: None,
                module: None,
            } if character_id == "character-8"
        ));
        let json_card: NativeFileJobStartRequest =
            serde_json::from_value(cases[1].clone()).unwrap();
        assert!(matches!(
            json_card,
            NativeFileJobStartRequest::ExportCharacterCard {
                portrait_rendition: None,
                destination: None,
                expected_revision: 9,
                character_id,
                format: CharacterCardExportFormat::JsonCard,
                metadata,
            } if character_id == "character-9"
                && metadata == json!({"spec": "chara_card_v3", "spec_version": "3.0"})
        ));
        let png_card: NativeFileJobStartRequest = serde_json::from_value(cases[2].clone()).unwrap();
        assert!(matches!(
            png_card,
            NativeFileJobStartRequest::ExportCharacterCard {
                portrait_rendition: None,
                destination: Some(destination),
                expected_revision: 10,
                character_id,
                format: CharacterCardExportFormat::PngCard,
                metadata,
            } if destination == "C:\\chosen\\character.png"
                && character_id == "character-10"
                && metadata == json!({"spec": "chara_card_v3", "spec_version": "3.0"})
        ));
        let risum: NativeFileJobStartRequest = serde_json::from_value(cases[3].clone()).unwrap();
        assert!(matches!(
            risum,
            NativeFileJobStartRequest::ExportRisuModule {
                destination: None,
                expected_revision: 11,
                module_index: 3,
            }
        ));
        let dataset: NativeFileJobStartRequest = serde_json::from_value(cases[4].clone()).unwrap();
        assert!(matches!(
            dataset,
            NativeFileJobStartRequest::ExportDataset {
                destination: Some(destination),
                expected_revision: 12,
            } if destination == "C:\\chosen\\dataset.json"
        ));

        assert_eq!(
            serde_json::to_value(JobKind::ExportCharacterCharx).unwrap(),
            json!("export-character-charx")
        );
        assert_eq!(
            serde_json::to_value(JobKind::ExportCharacterCard).unwrap(),
            json!("export-character-card")
        );
        assert_eq!(
            serde_json::to_value(JobKind::ExportRisuModule).unwrap(),
            json!("export-risu-module")
        );
        assert_eq!(
            serde_json::to_value(JobKind::ExportDataset).unwrap(),
            json!("export-dataset")
        );
    }

    #[test]
    fn official_publication_start_request_is_always_deserializable() {
        let request: NativeFileJobStartRequest = serde_json::from_value(serde_json::json!({
            "kind": "official-publication-upload",
            "lease": "snapshot-publication",
            "expectedRevision": 7,
            "accountId": "account-1",
            "baseUrl": "https://example.invalid",
            "replacements": { "local": "remote" },
            "session": "session-1",
            "saveDate": "save-date",
            "credential": { "kind": "risu-auth", "token": "private-token" }
        }))
        .unwrap();

        assert!(matches!(
            request,
            NativeFileJobStartRequest::OfficialPublicationUpload {
                lease,
                expected_revision: 7,
                account_id,
                base_url,
                replacements,
                session: Some(session),
                save_date,
                credential: OfficialPublicationCredential::RisuAuth { token },
            } if lease == "snapshot-publication"
                && account_id == "account-1"
                && base_url == "https://example.invalid"
                && replacements.get("local").map(String::as_str) == Some("remote")
                && session == "session-1"
                && save_date == "save-date"
                && token == "private-token"
        ));
    }

    #[cfg(not(feature = "native-official-publication"))]
    #[test]
    fn official_publication_retry_is_capability_unavailable_without_feature() {
        let directory = TempDir::new().unwrap();
        let state = NativeFileJobState::initialize(directory.path().to_path_buf());
        let error = state
            .retry_official_publication(OfficialPublicationRetryRequest {
                job_id: "missing-job".to_owned(),
                account_id: "account-1".to_owned(),
                session: Some("session".to_owned()),
                save_date: "date".to_owned(),
                credential: OfficialPublicationCredential::RisuAuth {
                    token: "token".to_owned(),
                },
            })
            .expect_err("feature-disabled retry must not inspect job state");

        assert_eq!(error.code, "capability-unavailable");
    }

    #[test]
    fn export_job_uses_its_own_ordered_phases() {
        let job = JobRegistry::default()
            .create(JobKind::ExportBlockRisuSave)
            .unwrap();

        job.start(JobPhase::WritingExport).unwrap();
        job.set_export_progress(JobProgress { completed_bytes: 100, total_bytes: None, ..Default::default() }, 100, None).unwrap();
        assert_eq!(job.status().detail.unwrap().stage_completed, 100);
        job.set_phase(JobPhase::PublishingDestination).unwrap();
        assert_eq!(job.status().detail.unwrap().stage_completed, 0);
        job.set_export_progress(JobProgress { completed_bytes: 125, total_bytes: Some(200), ..Default::default() }, 25, Some(100)).unwrap();
        let publishing = job.status().detail.unwrap();
        assert_eq!((publishing.stage_completed, publishing.stage_total), (25, Some(100)));
        job.commit_export_publication().unwrap();
        let finalizing = job.status().detail.unwrap();
        assert_eq!(finalizing.stage, JobStage::FinalizingExport);
        assert_eq!((finalizing.stage_completed, finalizing.stage_total), (0, None));

        assert_eq!(job.status().phase, JobPhase::FinalizingExport);
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::TooLate);
        assert!(job.set_phase(JobPhase::StagingDatabase).is_err());
    }

    #[test]
    fn accepted_cancel_prevents_destination_publication_commit() {
        let job = JobRegistry::default()
            .create(JobKind::ExportPortableBackup)
            .unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        job.set_phase(JobPhase::PublishingDestination).unwrap();
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::Requested);

        assert!(job.commit_export_publication().is_err());

        assert_eq!(job.status().state, JobState::Cancelling);
        assert_eq!(job.status().phase, JobPhase::PublishingDestination);
        assert!(job.is_cancel_requested());
    }

    #[test]
    fn accepted_cancel_maps_queued_and_prepublication_transitions_to_cancelled() {
        let queued = JobRegistry::default()
            .create(JobKind::ExportPortableBackup)
            .unwrap();
        assert_eq!(queued.request_cancel().unwrap(), CancelOutcome::Requested);
        assert_eq!(
            job_transition(&queued, queued.start(JobPhase::WritingExport))
                .unwrap_err()
                .code,
            "cancelled"
        );

        let preparing = JobRegistry::default()
            .create(JobKind::ExportPortableBackup)
            .unwrap();
        preparing.start(JobPhase::WritingExport).unwrap();
        assert_eq!(
            preparing.request_cancel().unwrap(),
            CancelOutcome::Requested
        );
        assert_eq!(
            job_transition(
                &preparing,
                preparing.set_phase(JobPhase::PublishingDestination),
            )
            .unwrap_err()
            .code,
            "cancelled"
        );
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
    fn device_job_cleanup_failure_keeps_the_terminal_receipt_for_retry() {
        let directory = tempfile::tempdir().unwrap();
        let state = NativeFileJobState::initialize(directory.path().to_owned());
        let device = crate::device_backup::DeviceBackupState::initialize(
            directory.path().join("device-backup"),
        );
        let pds = crate::persistent_store::PersistentStoreState::default();
        device
            .attach_maintenance_guard(pds.acquire_device_maintenance().unwrap())
            .unwrap();
        let job = state
            .registry
            .create(JobKind::ExportPortableBackup)
            .unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        let session = device
            .create_native_portable_session(
                &job.id(),
                false,
                &["hypa".into()],
                0,
                None,
            )
            .unwrap();
        job.set_device_session(&session).unwrap();
        job.finish_failure("synthetic", "synthetic failure")
            .unwrap();
        assert_eq!(
            forget_device_session(&state, &device, &job.id())
                .unwrap_err()
                .code,
            "cleanup-failed"
        );
        assert!(state.registry.lookup(&job.id()).unwrap().is_some());
        assert!(job.retains_terminal_receipt_until_forget());
    }
    #[test]
    fn terminal_status_rejects_unbounded_results_and_sanitizes_errors() {
        let registry = JobRegistry::default();
        let success = registry.create(JobKind::RestoreBlockRisuSave).unwrap();
        success.start(JobPhase::ReadingSource).unwrap();
        assert!(success
            .finish_success(JobResultSummary {
                export_exclusions: None,
                revision: 2,
                source_bytes: 128,
                source_sha256: "a".repeat(64),
                source_fingerprint_kind: crate::native_file_jobs::SourceFingerprintKind::WholeFileSha256,
                character_count: 1,
                preset_count: 1,
                warning_codes: (0..=MAX_WARNING_CODES)
                    .map(|index| format!("warning-{index}"))
                    .collect(),
                handoff_path: None,
                publication: None,
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
        let oversized = registry.create(JobKind::ExportCompatibleLocalBackup).unwrap();
        oversized.start(JobPhase::WritingExport).unwrap();
        oversized.finish_native_failure(&NativeJobError::too_large("compatible-database", "synthetic size limit")).unwrap();
        let status = serde_json::to_value(oversized.status()).unwrap();
        assert_eq!(status["error"]["code"], "payload-too-large");
        assert_eq!(status["error"]["itemKind"], "compatible-database");
        let rejected = registry.create(JobKind::ExportCompatibleLocalBackup).unwrap();
        rejected.start(JobPhase::WritingExport).unwrap();
        let owner = FailureOwner { kind: "chat", name: "Synthetic chat".into(), character: Some("Synthetic".into()) };
        rejected.finish_native_failure(&NativeJobError::new("unsupported-projection", "synthetic rejection").owned_by(&owner)).unwrap();
        assert_eq!(serde_json::to_value(rejected.status()).unwrap()["error"]["owner"],
            serde_json::json!({"kind":"chat","name":"Synthetic chat","character":"Synthetic"}));
        let content = registry.create(JobKind::PrepareContentImport).unwrap();
        content.start(JobPhase::ReadingSource).unwrap();
        content.finish_content_job_inner(Err(NativeJobError::too_large("card", "synthetic size limit")), Ok(()), None).unwrap();
        assert_eq!(serde_json::to_value(content.status()).unwrap()["error"]["itemKind"], "card");
    }

    #[test]
    fn restore_preview_keeps_recorded_omissions_with_the_counts_the_staging_adds() {
        let job = JobRegistry::default().create(JobKind::RestoreLegacyLocalBackup).unwrap();
        job.record_restore_omission("excludedCharacters", 2).unwrap();
        let mut staged = IncompleteRestorePreview::default();
        staged.omissions.inlays = 1;
        staged.omissions.missing_assets = 3;
        job.set_incomplete_restore_preview(staged).unwrap();
        assert_eq!(
            job.status().incomplete_restore_preview.unwrap().warning_codes(),
            ["skipped-backup-inlays:1", "excluded-backup-characters:2", "missing-backup-assets:3"],
        );
    }

    #[test]
    fn terminal_retention_is_bounded_by_count_and_age() {
        let registry = JobRegistry::with_retention(2, Duration::from_secs(60));
        let mut ids = Vec::new();
        for revision in 1..=3 {
            let job = registry.create(JobKind::ExportBlockRisuSave).unwrap();
            ids.push(job.id());
            job.start(JobPhase::WritingExport).unwrap();
            job.finish_success(result(revision)).unwrap();
        }

        assert!(registry.status(&ids[0]).is_err());
        assert!(registry.status(&ids[1]).is_ok());
        assert!(registry.status(&ids[2]).is_ok());

        let expiring = JobRegistry::with_retention(2, Duration::ZERO);
        let job = expiring.create(JobKind::ExportBlockRisuSave).unwrap();
        let id = job.id();
        job.start(JobPhase::WritingExport).unwrap();
        job.finish_success(result(1)).unwrap();
        assert!(expiring.status(&id).is_err());
    }

    fn detail(
        stage: JobStage,
        completed: u64,
        total: Option<u64>,
        counts: ImportCounts,
    ) -> JobDetail {
        JobDetail::new(stage, StageUnit::Items, completed, total, counts)
    }

    #[test]
    fn import_detail_only_moves_forward_and_keeps_counts_monotonic() {
        let job = JobRegistry::default()
            .create(JobKind::RestoreLegacyLocalBackup)
            .unwrap();
        assert!(job
            .set_detail(detail(
                JobStage::ReadingArchive,
                0,
                None,
                ImportCounts::default()
            ))
            .is_err());
        job.start(JobPhase::ReadingSource).unwrap();

        let mut counts = ImportCounts {
            entries_read: 3,
            assets: 2,
            ..ImportCounts::default()
        };
        job.set_detail(
            detail(JobStage::ReadingArchive, 10, Some(100), counts.clone())
                .with_current_item("assets/portrait.png"),
        )
        .unwrap();
        assert!(job
            .set_detail(detail(
                JobStage::ReadingArchive,
                9,
                Some(100),
                counts.clone()
            ))
            .is_err());
        assert!(job
            .set_detail(detail(
                JobStage::ReadingArchive,
                10,
                Some(200),
                counts.clone()
            ))
            .is_err());
        assert!(job
            .set_detail(detail(
                JobStage::ReadingArchive,
                500,
                Some(100),
                counts.clone()
            ))
            .is_err());
        counts.assets = 1;
        assert!(job
            .set_detail(detail(
                JobStage::ReadingArchive,
                20,
                Some(100),
                counts.clone()
            ))
            .is_err());
        counts.assets = 2;
        counts.entries_total = Some(3);
        job.set_detail(detail(
            JobStage::ReadingArchive,
            100,
            Some(100),
            counts.clone(),
        ))
        .unwrap();
        counts.entries_total = Some(4);
        assert!(job
            .set_detail(detail(
                JobStage::PreparingAttachments,
                0,
                Some(3),
                counts.clone()
            ))
            .is_err());
        counts.entries_total = Some(3);
        job.set_detail(detail(
            JobStage::PreparingAttachments,
            0,
            Some(2),
            counts.clone(),
        ))
        .unwrap();
        assert!(job
            .set_detail(detail(
                JobStage::ReadingArchive,
                100,
                Some(100),
                counts.clone()
            ))
            .is_err());
        job.set_detail(detail(
            JobStage::ReadingDatabase,
            5,
            Some(50),
            counts.clone(),
        ))
        .unwrap();

        let status = job.status();
        let current = status.detail.expect("detail retained");
        assert_eq!(current.stage, JobStage::ReadingDatabase);
        assert_eq!(current.stage_completed, 5);
        assert_eq!(current.counts.entries_total, Some(3));
        assert!(current.current_item.is_none());
    }

    #[test]
    fn import_detail_bounds_the_current_item_and_survives_terminal_states() {
        let job = JobRegistry::default()
            .create(JobKind::RestoreLegacyLocalBackup)
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        let long_name = format!("inlay/{}.webp", "x".repeat(400));
        job.set_detail(
            detail(JobStage::ReadingArchive, 1, None, ImportCounts::default())
                .with_current_item(long_name.clone()),
        )
        .unwrap();
        let item = job.status().detail.unwrap().current_item.unwrap();
        assert_eq!(item.chars().count(), MAX_CURRENT_ITEM_CHARS);
        assert!(item.starts_with('…'));
        assert!(item.ends_with(".webp"));

        job.finish_failure("invalid-source", "synthetic").unwrap();
        let terminal = job.status();
        assert_eq!(terminal.state, JobState::Failed);
        assert_eq!(terminal.detail.unwrap().stage, JobStage::ReadingArchive);
        assert!(job
            .set_detail(detail(
                JobStage::ReadingDatabase,
                0,
                None,
                ImportCounts::default()
            ))
            .is_err());
    }

    #[test]
    fn activation_hand_off_records_its_stages_with_the_gathered_counts() {
        let registry = JobRegistry::default();
        // Only jobs created with a restore context wait for the renderer's finalize call.
        let job = registry
            .create_with_context(JobKind::RestoreBlockRisuSave, Some(1), Vec::new())
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        let counts = ImportCounts {
            characters: 4,
            presets: 1,
            blocks: 9,
            ..ImportCounts::default()
        };
        job.set_detail(detail(JobStage::FinalizingStaging, 0, None, counts.clone()))
            .unwrap();
        job.set_phase(JobPhase::StagingDatabase).unwrap();

        let waiter = Arc::clone(&job);
        let handle = std::thread::spawn(move || waiter.wait_for_restore_finalization());
        loop {
            let status = job.status();
            if status.state == JobState::WaitingForInput {
                assert_eq!(
                    status.detail.as_ref().unwrap().stage,
                    JobStage::AwaitingActivation
                );
                assert_eq!(status.detail.as_ref().unwrap().counts, counts);
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            registry.finalize(&job.id(), None).unwrap(),
            FinalizeOutcome::Requested
        ));
        handle.join().unwrap().unwrap();
        let status = job.status();
        assert_eq!(status.phase, JobPhase::ActivatingDatabase);
        assert_eq!(status.detail.as_ref().unwrap().stage, JobStage::Activating);
        assert_eq!(status.detail.as_ref().unwrap().counts, counts);
    }

    #[test]
    fn failed_backup_job_records_diagnostic_code_and_failure_phase() {
        let job = JobRegistry::default()
            .create(JobKind::RestoreLegacyLocalBackup)
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();
        job.finish_failure("invalid-source", "synthetic backup rejected")
            .unwrap();
        let entry = crate::native_log::global_state()
            .tail(None)
            .into_iter()
            .find(|entry| entry.target == "native-file-job" && entry.message.contains(&job.id()));
        let entry =
            entry.expect("backup failure must remain in diagnostics after the progress UI closes");
        assert_eq!(entry.level, "error");
        assert!(entry.message.contains("invalid-source"));
        assert!(entry.message.contains("ReadingSource"));
        assert!(entry.message.contains("synthetic backup rejected"));
    }

    #[test]
    fn terminal_character_charx_receipt_is_retained_until_explicit_forget() {
        let registry = JobRegistry::with_retention(0, Duration::ZERO);
        let charx = registry.create(JobKind::ExportCharacterCharx).unwrap();
        let charx_id = charx.id();
        charx.start(JobPhase::WritingExport).unwrap();
        charx.finish_success(result(1)).unwrap();

        assert_eq!(
            registry.status(&charx_id).unwrap().state,
            JobState::Succeeded
        );
        assert!(registry.forget(&charx_id).unwrap());
        assert!(registry.status(&charx_id).is_err());
    }

    #[test]
    fn terminal_native_content_export_receipts_keep_exact_recovery_kinds_until_forget() {
        let registry = JobRegistry::with_retention(0, Duration::ZERO);
        for kind in [
            JobKind::ExportCharacterCard,
            JobKind::ExportRisuModule,
            JobKind::ExportDataset,
        ] {
            let job = registry.create(kind).unwrap();
            let id = job.id();
            job.start(JobPhase::WritingExport).unwrap();
            job.finish_success(result(1)).unwrap();

            assert_eq!(registry.status(&id).unwrap().kind, kind);
            assert!(registry.forget(&id).unwrap());
            assert!(registry.status(&id).is_err());
        }
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
                    owner_head: None,
                }),
                Ok(()),
            )
            .unwrap();

        let ordinary = registry.create(JobKind::ExportBlockRisuSave).unwrap();
        let ordinary_id = ordinary.id();
        ordinary.start(JobPhase::WritingExport).unwrap();
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
                    position: None,
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
                    position: None,
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
            owner_head: None,
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
    fn content_status_and_list_are_small_while_explicit_receipts_remain_retryable() {
        let directory = TempDir::new().unwrap();
        let state = NativeFileJobState::initialize(directory.path().join("native-file-jobs"));
        let job = state.registry.create(JobKind::PrepareContentImport).unwrap();
        let job_id = job.id();
        let mut content = retained_content_fixture(job_id.clone());
        content.metadata = serde_json::json!({"synthetic": "x".repeat(2 * 1024 * 1024)});
        job.start(JobPhase::ReadingSource).unwrap();
        job.finish_content_job(Ok(content.clone()), Ok(())).unwrap();

        for summary in [state.status_summary(&job_id).unwrap(), state.list().unwrap().remove(0)] {
            assert!(summary.prepared_content.is_none());
            let wire = serde_json::to_vec(&summary).unwrap();
            assert!(wire.len() < 4096);
            assert!(!String::from_utf8(wire).unwrap().contains("preparedContent"));
        }
        assert_eq!(state.prepared_content_receipt(&job_id).unwrap(), content);
        assert_eq!(state.prepared_content_receipt(&job_id).unwrap(), content);
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::Terminal);
        assert_eq!(state.prepared_content_receipt(&job_id).unwrap(), content);
        assert!(state.forget(&job_id).unwrap());
        assert!(state.prepared_content_receipt(&job_id).is_err());
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
                import_destination: None,
                operation_id: None,
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
                import_destination: None,
                operation_id: None,
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
                import_destination: None,
                operation_id: None,
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(
            open_job_source(directory.path(), &JobSource::AndroidSpool { token: token.clone() })
                .unwrap()
                .total_bytes,
            9,
        );
        for (operation_id, accepted) in [("a".repeat(64), true), ("A".repeat(64), false), ("a".repeat(63), false)] {
            fs::write(spool.join("source.json"), serde_json::to_vec(&SpoolManifest {
                token: token.clone(), state: SpoolState::Ready,
                display_name: "캐릭터\"😀.risudat".to_owned(), bytes: Some(9), total_bytes: Some(12),
                import_destination: None, operation_id: Some(operation_id),
            }).unwrap()).unwrap();
            assert_eq!(open_job_source(directory.path(), &JobSource::AndroidSpool { token: token.clone() }).is_ok(), accepted);
        }
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
            state.capability_error.lock().unwrap().as_ref().unwrap().code,
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
    fn handoff_cleanup_grammar_matches_the_shared_taxonomy_golden_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../src/ts/storage/tests/fixtures/nativeFileTaxonomyV1Golden.json"
        ))
        .unwrap();
        let uuid = fixture["uuid"].as_str().unwrap();
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("native-file-jobs");
        let handoffs = root.join("handoffs");
        fs::create_dir_all(&handoffs).unwrap();
        type Cleanup = fn(&Path, &Path) -> Result<bool, NativeJobError>;
        let cleanup_for = |kind: &str| -> Cleanup {
            match kind {
                "raw-recovery" => |root, path| {
                    cleanup_handoff_path(
                        root,
                        path,
                        "risunest-rescue-",
                        ".risunest-rescue.zip",
                        "raw recovery",
                    )
                },
                "portable-backup" => |root, path| {
                    cleanup_handoff_path(
                        root,
                        path,
                        "risunest-backup-",
                        ".risunest",
                        "portable backup",
                    )
                },
                "legacy-backup" => cleanup_legacy_backup_handoff_path,
                "character-charx" => cleanup_character_charx_handoff_path,
                "character-card" => cleanup_character_card_handoff_path,
                "risu-module" => cleanup_risu_module_handoff_path,
                "dataset-export" => cleanup_dataset_handoff_path,
                "download" => |root, path| {
                    cleanup_handoff_path(
                        root,
                        path,
                        download_handoff::PREFIX,
                        download_handoff::SUFFIX,
                        "download",
                    )
                },
                other => panic!("fixture grammar {other} has no cleanup entry point"),
            }
        };
        let grammars = fixture["managedHandoffs"].as_array().unwrap();
        assert_eq!(grammars.len(), 8, "grammar count drifted from the fixture");

        for grammar in grammars {
            let kind = grammar["kind"].as_str().unwrap();
            let prefix = grammar["prefix"].as_str().unwrap();
            let cleanup = cleanup_for(kind);
            for suffix in grammar["suffixes"].as_array().unwrap() {
                let suffix = suffix.as_str().unwrap();
                let owned = handoffs.join(format!("{prefix}{uuid}{suffix}"));
                fs::write(&owned, b"owned").unwrap();
                assert!(
                    cleanup(&root, &owned).unwrap(),
                    "{prefix}{uuid}{suffix} must be cleaned by {kind}"
                );
                assert!(!owned.exists());
                // Every other grammar must refuse this name.
                fs::write(&owned, b"owned").unwrap();
                for other in grammars {
                    let other_kind = other["kind"].as_str().unwrap();
                    if other_kind == kind {
                        continue;
                    }
                    assert_eq!(
                        cleanup_for(other_kind)(&root, &owned).unwrap_err().code,
                        "invalid-input",
                        "{other_kind} accepted {prefix}{uuid}{suffix}"
                    );
                }
                assert!(owned.exists());
                fs::remove_file(&owned).unwrap();
            }
        }

        for rejected in fixture["rejectedModuleNames"].as_array().unwrap() {
            let rejected = rejected.as_str().unwrap();
            let path = handoffs.join(rejected);
            fs::write(&path, b"rejected").unwrap();
            assert_eq!(
                cleanup_risu_module_handoff_path(&root, &path)
                    .unwrap_err()
                    .code,
                "invalid-input",
                "{rejected} must be rejected"
            );
            assert!(path.exists());
            fs::remove_file(&path).unwrap();
        }
    }

    #[test]
    fn legacy_backup_handoff_cleanup_removes_only_exact_owned_files_and_is_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path();
        let handoffs = root.join("handoffs");
        fs::create_dir_all(&handoffs).unwrap();
        let owned = handoffs.join(format!("risu-backup-{}.bin", Uuid::new_v4()));
        let unrelated = handoffs.join("keep.bin");
        fs::write(&owned, b"backup").unwrap();
        fs::write(&unrelated, b"keep").unwrap();

        assert!(cleanup_legacy_backup_handoff_path(root, &owned).unwrap());
        assert!(!cleanup_legacy_backup_handoff_path(root, &owned).unwrap());
        assert!(unrelated.exists());
        assert_eq!(
            cleanup_legacy_backup_handoff_path(root, &unrelated)
                .unwrap_err()
                .code,
            "invalid-input"
        );
    }

    #[test]
    fn native_file_lifecycle_startup_handoff_cleanup_reclaims_stale_owned_shapes() {
        let directory = TempDir::new().unwrap();
        let handoffs = directory.path().join("handoffs");
        fs::create_dir_all(&handoffs).unwrap();
        let uuid = Uuid::new_v4();
        let owned = [
            format!("risunest-backup-{uuid}.risunest"),
            format!("risu-backup-{uuid}.bin"),
            format!("risu-charx-{uuid}.charx"),
            format!("risu-charx-{uuid}.jpeg"),
            format!("risu-character-card-{uuid}.json"),
            format!("risu-character-card-{uuid}.png"),
            format!("risu-module-{uuid}.risum"),
            format!("risu-download-{uuid}.bin"),
            format!("risu-dataset-{uuid}.json"),
        ];
        for name in &owned {
            fs::write(handoffs.join(name), b"synthetic handoff").unwrap();
        }
        let unrelated = handoffs.join(format!("user-{uuid}.bin"));
        fs::write(&unrelated, b"keep").unwrap();
        let created = SystemTime::now();

        cleanup_stale_handoffs_at(&handoffs, created, HANDOFF_STALE_AFTER).unwrap();
        assert!(owned.iter().all(|name| handoffs.join(name).is_file()));

        cleanup_stale_handoffs_at(
            &handoffs,
            created + HANDOFF_STALE_AFTER + Duration::from_secs(1),
            HANDOFF_STALE_AFTER,
        )
        .unwrap();
        assert!(owned.iter().all(|name| !handoffs.join(name).exists()));
        assert!(unrelated.is_file());
    }

    #[test]
    fn native_file_lifecycle_legacy_handoff_jobs_are_retained_until_forget() {
        let registry = JobRegistry::with_retention(0, Duration::ZERO);
        for kind in [
            JobKind::ExportCompatibleLocalBackup,
        ] {
            let job = registry.create(kind).unwrap();
            job.start(JobPhase::WritingExport).unwrap();
            job.finish_success(result(1)).unwrap();
        }

        assert_eq!(registry.list().unwrap().len(), 1);
    }

    #[test]
    fn native_file_lifecycle_cancelling_job_retains_new_device_session_for_cleanup() {
        let job = JobRegistry::default()
            .create(JobKind::ExportPortableBackup)
            .unwrap();
        job.start(JobPhase::WritingExport).unwrap();
        assert_eq!(job.request_cancel().unwrap(), CancelOutcome::Requested);

        job.set_device_session("synthetic-device-session").unwrap();

        assert_eq!(
            job.status().device_session_id.as_deref(),
            Some("synthetic-device-session")
        );
        assert_eq!(job.status().state, JobState::Cancelling);
        assert_eq!(job.status().phase, JobPhase::WritingExport);
    }

    #[test]
    fn native_file_lifecycle_device_session_does_not_enter_renderer_wait() {
        let job = JobRegistry::default()
            .create(JobKind::RestorePortableBackup)
            .unwrap();
        job.start(JobPhase::ReadingSource).unwrap();

        job.set_device_session("synthetic-device-session").unwrap();

        assert_eq!(job.status().state, JobState::Running);
        assert_eq!(job.status().phase, JobPhase::ReadingSource);
        assert_eq!(
            job.status().device_session_id.as_deref(),
            Some("synthetic-device-session")
        );
    }

    #[test]
    fn native_file_lifecycle_worker_panics_become_bounded_store_errors() {
        let error = run_worker::<()>(
            || panic!("synthetic worker panic with private detail"),
            "native file job worker panicked",
        )
        .unwrap_err();

        assert_eq!(error.code, "store-error");
        assert_eq!(error.message, "native file job worker panicked");
    }

    #[test]
    fn native_file_lifecycle_registry_prune_recovers_from_poisoned_status() {
        let registry = JobRegistry::default();
        let job = registry.create(JobKind::ExportBlockRisuSave).unwrap();
        let poisoned = Arc::clone(&job);
        assert!(thread::spawn(move || {
            let _status = poisoned.status.lock().unwrap();
            panic!("synthetic status poison");
        })
        .join()
        .is_err());

        assert_eq!(registry.list().unwrap().len(), 1);
    }

    #[test]
    fn native_file_lifecycle_android_spool_manifest_requires_total_bytes_key() {
        let missing = br#"{"token":"token","state":"ready","displayName":"source.bin","bytes":1}"#;
        let nullable = br#"{"token":"token","state":"ready","displayName":"source.bin","bytes":1,"totalBytes":null}"#;

        assert!(serde_json::from_slice::<SpoolManifest>(missing).is_err());
        assert!(serde_json::from_slice::<SpoolManifest>(nullable).is_ok());
        let with_operation = serde_json::json!({
            "token": "token", "state": "ready", "displayName": "캐릭터.json", "bytes": 1,
            "totalBytes": null, "operationId": "a".repeat(64),
        });
        let parsed = serde_json::from_value::<SpoolManifest>(with_operation).unwrap();
        assert_eq!(parsed.operation_id.as_deref(), Some("a".repeat(64).as_str()));
    }

    #[test]
    fn character_charx_handoff_cleanup_removes_only_exact_owned_files_and_is_idempotent() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("native-file-jobs");
        let handoffs = root.join("handoffs");
        fs::create_dir_all(&handoffs).unwrap();
        let owned = handoffs.join(format!("risu-charx-{}.charx", Uuid::new_v4()));
        let unrelated = handoffs.join("keep.charx");
        fs::write(&owned, b"owned").unwrap();
        fs::write(&unrelated, b"unrelated").unwrap();

        assert!(cleanup_character_charx_handoff_path(&root, &owned).unwrap());
        assert!(!cleanup_character_charx_handoff_path(&root, &owned).unwrap());
        assert!(unrelated.is_file());
        assert_eq!(
            cleanup_character_charx_handoff_path(&root, &unrelated)
                .unwrap_err()
                .code,
            "invalid-input"
        );

        let jpeg = handoffs.join(format!("risu-charx-{}.jpeg", Uuid::new_v4()));
        fs::write(&jpeg, b"jpeg").unwrap();
        assert!(cleanup_character_charx_handoff_path(&root, &jpeg).unwrap());
        let wrong_extension = handoffs.join(format!("risu-charx-{}.jpg", Uuid::new_v4()));
        fs::write(&wrong_extension, b"jpg").unwrap();
        assert_eq!(
            cleanup_character_charx_handoff_path(&root, &wrong_extension)
                .unwrap_err()
                .code,
            "invalid-input"
        );
    }

    #[test]
    fn character_card_handoff_cleanup_removes_only_exact_owned_files_and_is_idempotent() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("native-file-jobs");
        let handoffs = root.join("handoffs");
        fs::create_dir_all(&handoffs).unwrap();
        let owned = handoffs.join(format!("risu-character-card-{}.json", Uuid::new_v4()));
        let unrelated = handoffs.join("character-card.json");
        fs::write(&owned, b"owned").unwrap();
        fs::write(&unrelated, b"unrelated").unwrap();

        assert!(cleanup_character_card_handoff_path(&root, &owned).unwrap());
        assert!(!cleanup_character_card_handoff_path(&root, &owned).unwrap());
        assert!(unrelated.is_file());
        assert_eq!(
            cleanup_character_card_handoff_path(&root, &unrelated)
                .unwrap_err()
                .code,
            "invalid-input"
        );

        let png = handoffs.join(format!("risu-character-card-{}.png", Uuid::new_v4()));
        fs::write(&png, b"owned PNG").unwrap();
        assert!(cleanup_character_card_handoff_path(&root, &png).unwrap());
        assert!(!cleanup_character_card_handoff_path(&root, &png).unwrap());
    }

    #[test]
    fn dataset_handoff_cleanup_removes_only_exact_owned_files_and_is_idempotent() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("native-file-jobs");
        let handoffs = root.join("handoffs");
        fs::create_dir_all(&handoffs).unwrap();
        let owned = handoffs.join(format!("risu-dataset-{}.json", Uuid::new_v4()));
        let unrelated = handoffs.join("dataset.json");
        fs::write(&owned, b"owned").unwrap();
        fs::write(&unrelated, b"unrelated").unwrap();

        assert!(cleanup_dataset_handoff_path(&root, &owned).unwrap());
        assert!(!cleanup_dataset_handoff_path(&root, &owned).unwrap());
        assert!(unrelated.is_file());
        assert_eq!(
            cleanup_dataset_handoff_path(&root, &unrelated)
                .unwrap_err()
                .code,
            "invalid-input"
        );
    }

    #[test]
    fn risu_module_handoff_cleanup_removes_only_exact_owned_files_and_is_idempotent() {
        let directory = TempDir::new().unwrap();
        let root = directory.path().join("native-file-jobs");
        let handoffs = root.join("handoffs");
        fs::create_dir_all(&handoffs).unwrap();
        let owned = handoffs.join(format!("risu-module-{}.risum", Uuid::new_v4()));
        let unrelated = handoffs.join("module.risum");
        fs::write(&owned, b"owned").unwrap();
        fs::write(&unrelated, b"unrelated").unwrap();

        assert!(cleanup_risu_module_handoff_path(&root, &owned).unwrap());
        assert!(!cleanup_risu_module_handoff_path(&root, &owned).unwrap());
        assert!(unrelated.is_file());
        assert_eq!(
            cleanup_risu_module_handoff_path(&root, &unrelated)
                .unwrap_err()
                .code,
            "invalid-input"
        );
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
                import_destination: None,
                operation_id: None,
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

    #[test]
    fn native_job_commands_log_their_own_failures() {
        let directory = TempDir::new().unwrap();
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        app.manage(NativeFileJobState::initialize(directory.path().join("native-file-jobs")));
        let refused = native_legacy_backup_handoff_cleanup(app.state(), "relative-handoff".into());
        assert!(refused.is_err());
        let refused = refused.unwrap_err().code;
        let selection = serde_json::from_value(json!({"library": true, "deviceSections": []})).unwrap();
        let missing = native_portable_select_sections(app.state(), "missing-job".into(), selection);
        assert_eq!(missing.unwrap_err().code, "job-not-found");
        let metadata = tauri::async_runtime::block_on(native_content_source_metadata(app.state(), "not-a-token".into()));
        let Err(metadata) = metadata else { panic!("an unknown token is refused") };
        let metadata = metadata.code;
        for (command, code) in [
            ("native_legacy_backup_handoff_cleanup", refused.as_str()),
            ("native_portable_select_sections", "job-not-found"),
            ("native_content_source_metadata", metadata.as_str()),
        ] {
            let entry = crate::native_log::global_state()
                .tail(None)
                .into_iter()
                .rev()
                .find(|entry| entry.message.starts_with(&format!("{command} failed: code={code} cause=")))
                .unwrap_or_else(|| panic!("{command} logs its failure"));
            assert_eq!((entry.level.as_str(), entry.target.as_str()), ("error", "native-command"));
            assert!(entry.message.contains("native_file_jobs"), "{}", entry.message);
        }
    }

    #[test]
    fn routine_native_job_refusals_are_warnings() {
        use crate::native_log::CommandFailure;
        for code in ["cancelled", "library-operation-busy", "job-capacity", "cleanup-pending", "revision-conflict"] {
            assert!(NativeJobError::new(code, "synthetic").expected(), "{code}");
        }
        for code in ["store-error", "invalid-input", "library-operation-state-unavailable", "job-not-found"] {
            let error = NativeJobError::new(code, "synthetic detail");
            assert!(!error.expected(), "{code}");
            assert_eq!((error.code().as_ref(), error.detail().as_deref()), (code, Some("synthetic detail")));
        }
    }
}

#[tauri::command]
pub(crate) async fn native_content_source_metadata(
    state: tauri::State<'_, NativeFileJobState>,
    token: String,
) -> Result<tauri::ipc::Response, NativeJobError> {
    logged("native_content_source_metadata", async move {
        let root = state.root.clone();
        tauri::async_runtime::spawn_blocking(move || {
            use std::io::Read;
            let source = open_job_source(&root, &JobSource::AndroidSpool { token })?;
            const LIMIT: u64 = crate::import_export_jobs::MAX_CONTENT_METADATA_BYTES as u64;
            if source.total_bytes > LIMIT {
                return Err(NativeJobError::too_large(
                    "content",
                    "Content metadata exceeds 128 MiB",
                ));
            }
            let mut bytes = Vec::new();
            source
                .file
                .take(LIMIT + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| NativeJobError::new("invalid-input", e.to_string()))?;
            if bytes.len() as u64 > LIMIT {
                return Err(NativeJobError::too_large(
                    "content",
                    "Content metadata exceeds 128 MiB",
                ));
            }
            std::str::from_utf8(&bytes)
                .map_err(|e| NativeJobError::new("invalid-input", e.to_string()))?;
            // Raw bytes take the streamed channel path on Android; a string
            // response would be evaluated as one script.
            Ok(tauri::ipc::Response::new(bytes))
        })
        .await
        .map_err(|e| NativeJobError::new("store-error", e.to_string()))?
    }.await)
}
