use super::PeerSyncError;
use serde::Serialize;
use std::{
    collections::HashMap,
    ffi::OsString,
    fs, io,
    path::{Component, Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::UNIX_EPOCH,
};
use tauri::{AppHandle, Manager};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerBackupInfo {
    pub(crate) path: String,
    pub(crate) bytes: u64,
    pub(crate) modified_at: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PeerTempUsage {
    pub(crate) count: u64,
    pub(crate) bytes: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub(crate) struct PeerBackupDeleteError {
    pub(crate) code: &'static str,
}

impl From<PeerSyncError> for PeerBackupDeleteError {
    fn from(error: PeerSyncError) -> Self {
        crate::nlog!("error", "peer_backup_delete failed: {error}");
        let code =
            matches!(error, PeerSyncError::Validation(message) if message == "peer-backup-in-use")
                .then_some("peer-backup-in-use")
                .unwrap_or("peer-backup-delete-failed");
        Self { code }
    }
}

fn active_temp_paths() -> &'static Mutex<HashMap<PathBuf, usize>> {
    static PATHS: OnceLock<Mutex<HashMap<PathBuf, usize>>> = OnceLock::new();
    PATHS.get_or_init(|| Mutex::new(HashMap::new()))
}

#[cfg(test)]
pub(crate) fn active_temp_registry_is_locked() -> bool {
    matches!(
        active_temp_paths().try_lock(),
        Err(std::sync::TryLockError::WouldBlock)
    )
}

fn normalize_temp_registry_path(path: &Path) -> Result<PathBuf, PeerSyncError> {
    let mut current = path;
    let mut suffix = Vec::<OsString>::new();
    loop {
        match fs::canonicalize(current) {
            Ok(mut normalized) => {
                for component in suffix.iter().rev() {
                    normalized.push(component);
                }
                return Ok(normalized);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let component = current.file_name().ok_or_else(|| {
                    PeerSyncError::Storage(
                        "active peer temp path has no existing ancestor".to_owned(),
                    )
                })?;
                suffix.push(component.to_os_string());
                current = current.parent().ok_or_else(|| {
                    PeerSyncError::Storage(
                        "active peer temp path has no existing ancestor".to_owned(),
                    )
                })?;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub(crate) struct ActiveTempGuard {
    path: PathBuf,
}

impl ActiveTempGuard {
    pub(crate) fn acquire(path: &Path) -> Result<Self, PeerSyncError> {
        let path = normalize_temp_registry_path(path)?;
        let mut active = active_temp_paths().lock().map_err(|error| {
            PeerSyncError::Storage(format!("active peer temp registry is poisoned: {error}"))
        })?;
        let count = active.entry(path.clone()).or_default();
        *count = count.saturating_add(1);
        Ok(Self { path })
    }
}

impl Drop for ActiveTempGuard {
    fn drop(&mut self) {
        let Ok(mut active) = active_temp_paths().lock() else {
            return;
        };
        let Some(count) = active.get_mut(&self.path) else {
            return;
        };
        *count = count.saturating_sub(1);
        if *count == 0 {
            active.remove(&self.path);
        }
    }
}

fn temp_path_is_active(path: &Path) -> Result<bool, PeerSyncError> {
    let path = normalize_temp_registry_path(path)?;
    active_temp_paths()
        .lock()
        .map(|active| active.contains_key(&path))
        .map_err(|error| {
            PeerSyncError::Storage(format!("active peer temp registry is poisoned: {error}"))
        })
}

pub(crate) fn link_like(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        return metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0;
    }
    #[cfg(not(windows))]
    {
        false
    }
}

fn regular_file(path: &Path) -> Option<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).ok()?;
    (metadata.is_file() && !link_like(&metadata)).then_some(metadata)
}

fn ordinary_directory(path: &Path) -> Option<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).ok()?;
    (metadata.is_dir() && !link_like(&metadata)).then_some(metadata)
}

#[derive(Clone, Copy)]
enum ExpectedPathKind {
    Directory,
    File,
}

struct DestructivePathBoundary {
    parent: fs::File,
    name: OsString,
}

impl DestructivePathBoundary {
    fn acquire(app_root: &Path, parent: &Path, target: &Path) -> Result<Self, PeerSyncError> {
        let parent_relative = plain_relative_path(app_root, parent)?;
        let name = target.strip_prefix(parent).map_err(|_| {
            PeerSyncError::Validation("maintenance target is outside its allowed root".to_owned())
        })?;
        if name.components().count() != 1
            || !matches!(name.components().next(), Some(Component::Normal(_)))
        {
            return Err(PeerSyncError::Validation(
                "maintenance target is not a direct child".to_owned(),
            ));
        }
        let name = name.as_os_str().to_os_string();
        let root = open_destructive_app_root(app_root)?;
        let metadata = root.metadata()?;
        if !metadata.is_dir() || link_like(&metadata) {
            return Err(PeerSyncError::Validation(
                "maintenance app root is not a plain directory".to_owned(),
            ));
        }
        let parent = cap_primitives::fs::open_dir_nofollow(&root, &parent_relative)?;
        let metadata = parent.metadata()?;
        if !metadata.is_dir() || link_like(&metadata) {
            return Err(PeerSyncError::Validation(
                "maintenance path traverses a link or reparse point".to_owned(),
            ));
        }
        Ok(Self { parent, name })
    }

    fn remove_file(&self) -> Result<(), PeerSyncError> {
        cap_primitives::fs::remove_file(&self.parent, Path::new(&self.name))?;
        Ok(())
    }

    fn remove_dir_all(&self) -> Result<(), PeerSyncError> {
        cap_primitives::fs::remove_dir_all(&self.parent, Path::new(&self.name))?;
        Ok(())
    }
}

#[cfg(unix)]
fn open_destructive_app_root(app_root: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW);
    options.open(app_root)
}

#[cfg(windows)]
fn open_destructive_app_root(app_root: &Path) -> io::Result<fs::File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    options.open(app_root)
}

#[cfg(not(any(unix, windows)))]
fn open_destructive_app_root(app_root: &Path) -> io::Result<fs::File> {
    fs::File::open(app_root)
}

fn plain_relative_path(app_root: &Path, target: &Path) -> Result<PathBuf, PeerSyncError> {
    let relative = target.strip_prefix(app_root).map_err(|_| {
        PeerSyncError::Validation("maintenance path is outside the app root".to_owned())
    })?;
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(PeerSyncError::Validation(
            "maintenance path is not a plain descendant".to_owned(),
        ));
    }
    Ok(relative.to_path_buf())
}

fn validate_existing_plain_path(
    app_root: &Path,
    target: &Path,
    expected: ExpectedPathKind,
) -> Result<Option<PathBuf>, PeerSyncError> {
    let app_metadata = fs::symlink_metadata(app_root)
        .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    if !app_metadata.is_dir() || link_like(&app_metadata) {
        return Err(PeerSyncError::Validation(
            "maintenance app root is not a plain directory".to_owned(),
        ));
    }
    let canonical_app =
        fs::canonicalize(app_root).map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    let relative = plain_relative_path(app_root, target)?;

    let mut current = app_root.to_path_buf();
    let component_count = relative.components().count();
    for (index, component) in relative.components().enumerate() {
        let Component::Normal(component) = component else {
            unreachable!();
        };
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if link_like(&metadata) {
            return Err(PeerSyncError::Validation(
                "maintenance path traverses a link or reparse point".to_owned(),
            ));
        }
        let final_component = index + 1 == component_count;
        if (!final_component && !metadata.is_dir())
            || (final_component
                && match expected {
                    ExpectedPathKind::Directory => !metadata.is_dir(),
                    ExpectedPathKind::File => !metadata.is_file(),
                })
        {
            return Err(PeerSyncError::Validation(
                "maintenance path has an unexpected file type".to_owned(),
            ));
        }
        let canonical = fs::canonicalize(&current)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        if !canonical.starts_with(&canonical_app) {
            return Err(PeerSyncError::Validation(
                "maintenance path escapes the app root".to_owned(),
            ));
        }
    }
    Ok(Some(target.to_path_buf()))
}

fn backup_roots(app_root: &Path) -> [PathBuf; 3] {
    [
        app_root.join("peer-clone/activation/backups"),
        app_root.join("peer-bidirectional/backups"),
        app_root.join("peer-clone-activation/backups"),
    ]
}

pub(crate) fn list_backups(app_root: &Path) -> Result<Vec<PeerBackupInfo>, PeerSyncError> {
    let mut backups = Vec::new();
    for root in backup_roots(app_root) {
        let Some(root) =
            validate_existing_plain_path(app_root, &root, ExpectedPathKind::Directory)?
        else {
            continue;
        };
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let path = entry.path();
            let Some(metadata) = regular_file(&path) else {
                continue;
            };
            let modified_at = metadata
                .modified()?
                .duration_since(UNIX_EPOCH)
                .map_err(|e| PeerSyncError::Storage(e.to_string()))?
                .as_millis() as u64;
            backups.push(PeerBackupInfo {
                path: path.to_string_lossy().into_owned(),
                bytes: metadata.len(),
                modified_at,
            });
        }
    }
    backups.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.path.cmp(&b.path))
    });
    Ok(backups)
}

fn normalize_operation_path(app_root: &Path, path: &str) -> Result<PathBuf, PeerSyncError> {
    let path = Path::new(path);
    let candidate = if path.is_absolute() {
        path.to_owned()
    } else {
        app_root.join(path)
    };
    let candidate = validate_existing_plain_path(app_root, &candidate, ExpectedPathKind::File)?
        .ok_or_else(|| {
            PeerSyncError::Storage("bidirectional backup reference is missing".to_owned())
        })?;
    fs::canonicalize(candidate).map_err(|error| PeerSyncError::Storage(error.to_string()))
}

fn load_bidirectional_operation(
    app_root: &Path,
) -> Result<Option<super::bidirectional_commands::PeerBidirectionalDurableOperation>, PeerSyncError>
{
    let operation_root = app_root.join("peer-bidirectional");
    let Some(_) =
        validate_existing_plain_path(app_root, &operation_root, ExpectedPathKind::Directory)?
    else {
        return Ok(None);
    };
    let operation_path = operation_root.join("operation.json");
    if operation_path.exists() {
        validate_existing_plain_path(app_root, &operation_path, ExpectedPathKind::File)?
            .ok_or_else(|| {
                PeerSyncError::Storage("bidirectional operation journal is missing".to_owned())
            })?;
    }
    let journal = super::bidirectional_commands::PeerBidirectionalOperationJournal::new(app_root);
    journal.load()
}

fn operation_references(app_root: &Path, candidate: &Path) -> Result<bool, PeerSyncError> {
    let Some(operation) = load_bidirectional_operation(app_root)? else {
        return Ok(false);
    };
    let candidate =
        fs::canonicalize(candidate).map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    for path in operation.backup_paths() {
        if normalize_operation_path(app_root, path)? == candidate {
            return Ok(true);
        }
    }
    Ok(false)
}

fn validated_direct_child(
    app_root: &Path,
    root: &Path,
    target: &Path,
    file: bool,
) -> Result<PathBuf, PeerSyncError> {
    let root = validate_existing_plain_path(app_root, root, ExpectedPathKind::Directory)?
        .ok_or_else(|| PeerSyncError::Validation("maintenance root is missing".to_owned()))?;
    let target = validate_existing_plain_path(
        app_root,
        target,
        if file {
            ExpectedPathKind::File
        } else {
            ExpectedPathKind::Directory
        },
    )?
    .ok_or_else(|| PeerSyncError::Validation("maintenance target is missing".to_owned()))?;
    if target.parent() != Some(root.as_path()) {
        return Err(PeerSyncError::Validation(
            "maintenance target escapes its root".to_owned(),
        ));
    }
    Ok(target)
}

pub(crate) fn desktop_clone_backup_job_is_active(app_root: &Path, backup: &Path) -> bool {
    let Some(name) = backup.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(stem) = name
        .strip_prefix("pre-clone-")
        .and_then(|name| name.strip_suffix(".lossless"))
    else {
        return false;
    };
    let job_id = if stem.len() == 36 {
        stem
    } else if stem.len() == 101
        && stem.as_bytes()[..64].iter().all(u8::is_ascii_hexdigit)
        && stem.as_bytes()[64] == b'-'
    {
        &stem[65..]
    } else {
        return false;
    };
    let Ok(parsed) = uuid::Uuid::parse_str(job_id) else {
        return false;
    };
    if parsed.to_string() != job_id {
        return false;
    }
    let Ok(job) = crate::asset_repository::job_pins::DurableCasJob::open(app_root, job_id) else {
        return false;
    };
    job.kind() == crate::asset_repository::job_pins::CasJobKind::PeerClone && !job.is_released()
}

fn clone_backup_root(app_root: &Path, root: &Path) -> bool {
    root == app_root.join("peer-clone/activation/backups")
        || root == app_root.join("peer-clone-activation/backups")
}

fn clone_backup_is_in_use(
    app_root: &Path,
    root: &Path,
    backup: &Path,
) -> Result<bool, PeerSyncError> {
    if !clone_backup_root(app_root, root) {
        return Ok(false);
    }
    if desktop_clone_backup_job_is_active(app_root, backup) {
        return Ok(true);
    }
    #[cfg(any(desktop, test))]
    {
        if root == app_root.join("peer-clone/activation/backups")
            && super::commands::retryable_target_operation_references_backup(
                &app_root.join("peer-clone"),
                backup,
            )?
        {
            return Ok(true);
        }
    }
    #[cfg(any(target_os = "android", test))]
    {
        if root == app_root.join("peer-clone-activation/backups")
            && super::android_client::current_android_clone_job_references_backup(app_root, backup)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn clone_activation_stage_job_is_active(app_root: &Path, stage: &Path) -> bool {
    let Some((_, job_id)) = super::production::activation_stage_identity(stage) else {
        return false;
    };
    let Ok(job) = crate::asset_repository::job_pins::DurableCasJob::open(app_root, &job_id) else {
        return false;
    };
    job.kind() == crate::asset_repository::job_pins::CasJobKind::PeerClone && !job.is_released()
}

pub(crate) fn delete_backup(app_root: &Path, requested: &Path) -> Result<(), PeerSyncError> {
    delete_backup_with_predelete_hooks_inner(app_root, requested, || Ok(()), || Ok(()))
}

#[cfg(test)]
pub(crate) fn delete_backup_with_predelete_hook(
    app_root: &Path,
    requested: &Path,
    hook: impl FnMut() -> Result<(), PeerSyncError>,
) -> Result<(), PeerSyncError> {
    delete_backup_with_predelete_hooks_inner(app_root, requested, hook, || Ok(()))
}

#[cfg(test)]
pub(crate) fn delete_backup_with_postvalidation_hook(
    app_root: &Path,
    requested: &Path,
    hook: impl FnMut() -> Result<(), PeerSyncError>,
) -> Result<(), PeerSyncError> {
    delete_backup_with_predelete_hooks_inner(app_root, requested, || Ok(()), hook)
}

fn delete_backup_with_predelete_hooks_inner(
    app_root: &Path,
    requested: &Path,
    mut hook: impl FnMut() -> Result<(), PeerSyncError>,
    mut postvalidation_hook: impl FnMut() -> Result<(), PeerSyncError>,
) -> Result<(), PeerSyncError> {
    let requested = fs::canonicalize(requested)
        .map_err(|_| PeerSyncError::Validation("peer backup is not currently listed".to_owned()))?;
    let listed = list_backups(app_root)?;
    let selected = listed
        .into_iter()
        .find_map(|item| {
            fs::canonicalize(&item.path)
                .ok()
                .filter(|path| path == &requested)
                .map(|_| PathBuf::from(item.path))
        })
        .ok_or_else(|| {
            PeerSyncError::Validation("peer backup is not currently listed".to_owned())
        })?;
    if operation_references(app_root, &selected)? {
        return Err(PeerSyncError::Validation("peer-backup-in-use".to_owned()));
    }
    let root = backup_roots(app_root)
        .into_iter()
        .find(|root| validated_direct_child(app_root, root, &selected, true).is_ok())
        .ok_or_else(|| {
            PeerSyncError::Validation("peer backup root is no longer allowed".to_owned())
        })?;
    let selected = validated_direct_child(app_root, &root, &selected, true)?;
    if operation_references(app_root, &selected)? {
        return Err(PeerSyncError::Validation("peer-backup-in-use".to_owned()));
    }
    let selected = validated_direct_child(app_root, &root, &selected, true)?;
    if clone_backup_is_in_use(app_root, &root, &selected)? {
        return Err(PeerSyncError::Validation("peer-backup-in-use".to_owned()));
    }
    hook()?;
    let selected = validated_direct_child(app_root, &root, &selected, true)?;
    let boundary = DestructivePathBoundary::acquire(app_root, &root, &selected)?;
    let selected = validated_direct_child(app_root, &root, &selected, true)?;
    if operation_references(app_root, &selected)? {
        return Err(PeerSyncError::Validation("peer-backup-in-use".to_owned()));
    }
    if clone_backup_is_in_use(app_root, &root, &selected)? {
        return Err(PeerSyncError::Validation("peer-backup-in-use".to_owned()));
    }
    postvalidation_hook()?;
    boundary.remove_file()?;
    Ok(())
}

fn temp_roots(app_root: &Path) -> Vec<(PathBuf, bool)> {
    vec![
        (app_root.join("peer-clone/activation"), true),
        (app_root.join("peer-clone-activation"), true),
        (app_root.join("peer-bidirectional/staging"), false),
        (app_root.join("peer-bidirectional/backup-staging"), false),
        (app_root.join("peer-delta/staging"), false),
    ]
}

fn temp_candidates(app_root: &Path) -> Result<Vec<(PathBuf, PathBuf)>, PeerSyncError> {
    let mut candidates = Vec::new();
    for (root, clone_activation) in temp_roots(app_root) {
        let Some(validated_root) =
            validate_existing_plain_path(app_root, &root, ExpectedPathKind::Directory)?
        else {
            continue;
        };
        for entry in fs::read_dir(&validated_root)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if clone_activation && (name == "backups" || name == "pre-replacement-backup") {
                continue;
            }
            if ordinary_directory(&path).is_none()
                || temp_path_is_active(&root.join(&name))?
                || path.join("operation.json").exists()
                || operation_references(app_root, &path)?
                || (clone_activation && clone_activation_stage_job_is_active(app_root, &path))
            {
                continue;
            }
            candidates.push((root.clone(), path));
        }
    }
    Ok(candidates)
}

fn tree_usage(path: &Path) -> Result<PeerTempUsage, PeerSyncError> {
    let mut usage = PeerTempUsage::default();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)?;
        if link_like(&metadata) {
            continue;
        }
        if metadata.is_file() {
            usage.count += 1;
            usage.bytes = usage.bytes.saturating_add(metadata.len());
        } else if metadata.is_dir() {
            let nested = tree_usage(&child)?;
            usage.count += nested.count;
            usage.bytes = usage.bytes.saturating_add(nested.bytes);
        }
    }
    Ok(usage)
}

pub(crate) fn temp_usage(app_root: &Path) -> Result<PeerTempUsage, PeerSyncError> {
    temp_candidates(app_root)?
        .iter()
        .try_fold(PeerTempUsage::default(), |mut total, (_, path)| {
            let usage = tree_usage(path)?;
            total.count += usage.count;
            total.bytes = total.bytes.saturating_add(usage.bytes);
            Ok(total)
        })
}

pub(crate) fn cleanup_temp(app_root: &Path) -> Result<PeerTempUsage, PeerSyncError> {
    cleanup_temp_with_predelete_hooks_inner(app_root, |_, _| Ok(()), |_, _| Ok(()), |_, _| Ok(()))
}

#[cfg(test)]
pub(crate) fn cleanup_temp_with_predelete_hook(
    app_root: &Path,
    hook: impl FnMut(&Path, &Path) -> Result<(), PeerSyncError>,
) -> Result<PeerTempUsage, PeerSyncError> {
    cleanup_temp_with_predelete_hooks_inner(app_root, hook, |_, _| Ok(()), |_, _| Ok(()))
}

#[cfg(test)]
pub(crate) fn cleanup_temp_with_locked_predelete_hook(
    app_root: &Path,
    hook: impl FnMut(&Path, &Path) -> Result<(), PeerSyncError>,
) -> Result<PeerTempUsage, PeerSyncError> {
    cleanup_temp_with_predelete_hooks_inner(app_root, |_, _| Ok(()), hook, |_, _| Ok(()))
}

#[cfg(test)]
pub(crate) fn cleanup_temp_with_postvalidation_hook(
    app_root: &Path,
    hook: impl FnMut(&Path, &Path) -> Result<(), PeerSyncError>,
) -> Result<PeerTempUsage, PeerSyncError> {
    cleanup_temp_with_predelete_hooks_inner(app_root, |_, _| Ok(()), |_, _| Ok(()), hook)
}

fn cleanup_temp_with_predelete_hooks_inner(
    app_root: &Path,
    mut hook: impl FnMut(&Path, &Path) -> Result<(), PeerSyncError>,
    mut locked_hook: impl FnMut(&Path, &Path) -> Result<(), PeerSyncError>,
    mut postvalidation_hook: impl FnMut(&Path, &Path) -> Result<(), PeerSyncError>,
) -> Result<PeerTempUsage, PeerSyncError> {
    let candidates = temp_candidates(app_root)?;
    let mut removed = PeerTempUsage::default();
    for (root, path) in candidates {
        let usage = tree_usage(&path)?;
        let original_path = root.join(path.file_name().ok_or_else(|| {
            PeerSyncError::Validation("maintenance target has no name".to_owned())
        })?);
        if temp_path_is_active(&original_path)? {
            continue;
        }
        let path = validated_direct_child(app_root, &root, &path, false)?;
        if operation_references(app_root, &path)? || temp_path_is_active(&original_path)? {
            continue;
        }
        hook(&root, &path)?;
        let path = validated_direct_child(app_root, &root, &path, false)?;
        if operation_references(app_root, &path)? || temp_path_is_active(&original_path)? {
            continue;
        }
        if clone_activation_stage_job_is_active(app_root, &path) {
            continue;
        }
        let registry_path = normalize_temp_registry_path(&original_path)?;
        let active = active_temp_paths().lock().map_err(|error| {
            PeerSyncError::Storage(format!("active peer temp registry is poisoned: {error}"))
        })?;
        if active.contains_key(&registry_path) {
            continue;
        }
        locked_hook(&root, &path)?;
        let path = validated_direct_child(app_root, &root, &path, false)?;
        let boundary = DestructivePathBoundary::acquire(app_root, &root, &path)?;
        let path = validated_direct_child(app_root, &root, &path, false)?;
        postvalidation_hook(&root, &path)?;
        boundary.remove_dir_all()?;
        drop(active);
        removed.count += usage.count;
        removed.bytes = removed.bytes.saturating_add(usage.bytes);
    }
    Ok(removed)
}

fn app_root(app: &AppHandle) -> Result<PathBuf, PeerSyncError> {
    app.path()
        .app_data_dir()
        .map_err(|error| PeerSyncError::Storage(error.to_string()))
}

fn finish_string_command<T>(
    command: &'static str,
    result: Result<T, PeerSyncError>,
) -> Result<T, String> {
    result.map_err(|error| {
        crate::nlog!("error", "{command} failed: {error}");
        error.to_string()
    })
}

pub(crate) fn peer_backup_list_from_root(
    root: Result<PathBuf, PeerSyncError>,
) -> Result<Vec<PeerBackupInfo>, String> {
    finish_string_command(
        "peer_backup_list",
        root.and_then(|root| list_backups(&root)),
    )
}

pub(crate) fn peer_backup_delete_from_root(
    root: Result<PathBuf, PeerSyncError>,
    path: &Path,
) -> Result<(), PeerBackupDeleteError> {
    root.and_then(|root| delete_backup(&root, path))
        .map_err(PeerBackupDeleteError::from)
}

pub(crate) fn peer_temp_usage_from_root(
    root: Result<PathBuf, PeerSyncError>,
) -> Result<PeerTempUsage, String> {
    finish_string_command("peer_temp_usage", root.and_then(|root| temp_usage(&root)))
}

pub(crate) fn peer_temp_cleanup_from_root(
    root: Result<PathBuf, PeerSyncError>,
) -> Result<PeerTempUsage, String> {
    finish_string_command(
        "peer_temp_cleanup",
        root.and_then(|root| cleanup_temp(&root)),
    )
}

#[tauri::command(async)]
pub(crate) fn peer_backup_list(app: AppHandle) -> Result<Vec<PeerBackupInfo>, String> {
    peer_backup_list_from_root(app_root(&app))
}

#[tauri::command(async)]
pub(crate) fn peer_backup_delete(
    app: AppHandle,
    path: String,
) -> Result<(), PeerBackupDeleteError> {
    peer_backup_delete_from_root(app_root(&app), Path::new(&path))
}

#[tauri::command(async)]
pub(crate) fn peer_temp_usage(app: AppHandle) -> Result<PeerTempUsage, String> {
    peer_temp_usage_from_root(app_root(&app))
}

#[tauri::command(async)]
pub(crate) fn peer_temp_cleanup(app: AppHandle) -> Result<PeerTempUsage, String> {
    peer_temp_cleanup_from_root(app_root(&app))
}
