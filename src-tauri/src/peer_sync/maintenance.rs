use super::PeerSyncError;
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
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

fn link_like(metadata: &fs::Metadata) -> bool {
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
        if ordinary_directory(&root).is_none() {
            continue;
        }
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

fn operation_references(app_root: &Path, candidate: &Path) -> bool {
    let wanted = candidate.to_string_lossy();
    [
        app_root.join("peer-bidirectional/operation.json"),
        app_root.join("peer-clone/operation.json"),
    ]
    .iter()
    .any(|path| {
        regular_file(path).is_some_and(|metadata| metadata.len() <= 1_048_576)
            && fs::read_to_string(path)
                .map(|body| body.contains(wanted.as_ref()))
                .unwrap_or(true)
    })
}

fn clone_job_exists(app_root: &Path) -> bool {
    let root = app_root.join("assets-v2/job-pins");
    ordinary_directory(&root).is_some_and(|_| {
        fs::read_dir(root).ok().is_some_and(|entries| {
            entries
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("job-"))
        })
    })
}

pub(crate) fn delete_backup(app_root: &Path, requested: &Path) -> Result<(), PeerSyncError> {
    let requested = fs::canonicalize(requested)
        .map_err(|_| PeerSyncError::Validation("peer backup is not currently listed".to_owned()))?;
    let listed = list_backups(app_root)?;
    let selected = listed
        .into_iter()
        .find_map(|item| {
            fs::canonicalize(item.path)
                .ok()
                .filter(|path| path == &requested)
        })
        .ok_or_else(|| {
            PeerSyncError::Validation("peer backup is not currently listed".to_owned())
        })?;
    if operation_references(app_root, &selected)
        || (selected.starts_with(app_root.join("peer-clone")) && clone_job_exists(app_root))
    {
        return Err(PeerSyncError::Validation("peer-backup-in-use".to_owned()));
    }
    // Recheck after all validation, so a replacement cannot turn the target into a link.
    let metadata = regular_file(&selected).ok_or_else(|| {
        PeerSyncError::Validation("peer backup changed before deletion".to_owned())
    })?;
    if metadata.len() == 0 && !selected.exists() {
        return Ok(());
    }
    fs::remove_file(selected)?;
    Ok(())
}

fn temp_roots(app_root: &Path) -> Vec<(PathBuf, bool)> {
    vec![
        (app_root.join("peer-clone/activation"), true),
        (app_root.join("peer-bidirectional/staging"), false),
        (app_root.join("peer-bidirectional/backup-staging"), false),
        (app_root.join("peer-delta/staging"), false),
    ]
}

fn temp_candidates(app_root: &Path) -> Result<Vec<PathBuf>, PeerSyncError> {
    let mut candidates = Vec::new();
    for (root, clone_activation) in temp_roots(app_root) {
        if ordinary_directory(&root).is_none() {
            continue;
        }
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name();
            if clone_activation && (name == "backups" || name == "pre-replacement-backup") {
                continue;
            }
            if ordinary_directory(&path).is_none()
                || path.join("operation.json").exists()
                || operation_references(app_root, &path)
            {
                continue;
            }
            candidates.push(path);
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
        .try_fold(PeerTempUsage::default(), |mut total, path| {
            let usage = tree_usage(path)?;
            total.count += usage.count;
            total.bytes = total.bytes.saturating_add(usage.bytes);
            Ok(total)
        })
}

pub(crate) fn cleanup_temp(app_root: &Path) -> Result<PeerTempUsage, PeerSyncError> {
    let candidates = temp_candidates(app_root)?;
    let mut removed = PeerTempUsage::default();
    for path in candidates {
        let usage = tree_usage(&path)?;
        if ordinary_directory(&path).is_none() || operation_references(app_root, &path) {
            continue;
        }
        fs::remove_dir_all(&path)?;
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

#[tauri::command(async)]
pub(crate) fn peer_backup_list(app: AppHandle) -> Result<Vec<PeerBackupInfo>, PeerSyncError> {
    list_backups(&app_root(&app)?)
}

#[tauri::command(async)]
pub(crate) fn peer_backup_delete(app: AppHandle, path: String) -> Result<(), PeerSyncError> {
    delete_backup(&app_root(&app)?, Path::new(&path))
}

#[tauri::command(async)]
pub(crate) fn peer_temp_usage(app: AppHandle) -> Result<PeerTempUsage, PeerSyncError> {
    temp_usage(&app_root(&app)?)
}

#[tauri::command(async)]
pub(crate) fn peer_temp_cleanup(app: AppHandle) -> Result<PeerTempUsage, PeerSyncError> {
    cleanup_temp(&app_root(&app)?)
}
