use super::write_json;
use crate::Result;
use serde::{Deserialize, Serialize};
#[cfg(windows)]
use std::fs::OpenOptions;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

const TRANSACTION_SCHEMA: &str = "risunest-sync-install-transaction/v1";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransactionKind {
    Directory,
    Files,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransactionPhase {
    Prepared,
    BackupMoved,
    Installing,
    Installed,
    Restarting,
    Completed,
    RollingBack,
    RolledBack,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum FilePhase {
    Pending,
    Prepared,
    BackedUp,
    Installed,
    Restored,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileTransaction {
    pub path: PathBuf,
    pub desired_present: bool,
    pub original_existed: Option<bool>,
    pub phase: FilePhase,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DirectoryIdentity {
    device: u64,
    inode: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecoveryOutcome {
    Completed {
        target_version: String,
    },
    RolledBack {
        was_running: bool,
        source_version: String,
        target_version: String,
        installer_startup_enabled: Option<bool>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InstallTransaction {
    pub schema: String,
    pub source_version: String,
    pub target_version: String,
    pub kind: TransactionKind,
    pub phase: TransactionPhase,
    pub install_path: PathBuf,
    pub staged_path: PathBuf,
    pub backup_path: PathBuf,
    pub file_states: Vec<FileTransaction>,
    pub was_running: bool,
    installer_startup_enabled: Option<bool>,
    directory_install_identity: Option<DirectoryIdentity>,
    directory_staged_identity: Option<DirectoryIdentity>,
}

fn state_path(root: &Path) -> PathBuf {
    root.join("manager-update/transaction.json")
}

fn validate_relative(path: &Path) -> Result<()> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err("update-transaction-invalid".into());
    }
    if path
        .components()
        .any(|part| !matches!(part, std::path::Component::Normal(_)))
    {
        return Err("update-transaction-invalid".into());
    }
    Ok(())
}

fn validate_managed_sibling(install: &Path, path: &Path) -> Result<()> {
    let same_parent = install.parent().is_some() && install.parent() == path.parent();
    let managed_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with(".risunest-sync-update-"));
    if !same_parent || !managed_name {
        return Err("update-transaction-invalid".into());
    }
    Ok(())
}

fn is_reparse_or_symlink(path: &Path) -> Result<bool> {
    let metadata =
        fs::symlink_metadata(path).map_err(|_| "update-transaction-invalid".to_owned())?;
    if metadata.file_type().is_symlink() {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Ok(metadata.file_attributes() & 0x400 != 0)
    }
    #[cfg(not(windows))]
    Ok(false)
}

fn reject_linked_path(root: &Path, relative: &Path) -> Result<()> {
    let mut current = root.to_owned();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err("update-transaction-invalid".into());
        };
        current.push(component);
        if current.exists() && is_reparse_or_symlink(&current)? {
            return Err("update-install-path-linked".into());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("path has no parent"))?;
    fs::File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn sync_parent(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn durable_rename(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)?;
    sync_parent(source)?;
    if source.parent() != target.parent() {
        sync_parent(target)?;
    }
    Ok(())
}

#[cfg(windows)]
fn durable_rename(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
fn durable_replace(target: &Path, replacement: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }
    let target = wide(target);
    let replacement = wide(replacement);
    if unsafe {
        MoveFileExW(
            replacement.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn durable_backup_copy(source: &Path, backup: &Path) -> io::Result<()> {
    let mut temporary_name = backup.as_os_str().to_os_string();
    temporary_name.push(".pending");
    let temporary = PathBuf::from(temporary_name);
    let result = (|| {
        let mut input = fs::File::open(source)?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        durable_rename(&temporary, backup)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

#[cfg(unix)]
fn durable_replace(target: &Path, replacement: &Path) -> io::Result<()> {
    durable_rename(replacement, target)
}

#[cfg(target_os = "linux")]
fn durable_exchange(left: &Path, right: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let left_name = CString::new(left.as_os_str().as_bytes())?;
    let right_name = CString::new(right.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            left_name.as_ptr(),
            libc::AT_FDCWD,
            right_name.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    sync_parent(left)?;
    if left.parent() != right.parent() {
        sync_parent(right)?;
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn durable_exchange(left: &Path, right: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};

    let left_name = CString::new(left.as_os_str().as_bytes())?;
    let right_name = CString::new(right.as_os_str().as_bytes())?;
    if unsafe { libc::renamex_np(left_name.as_ptr(), right_name.as_ptr(), libc::RENAME_SWAP) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    sync_parent(left)?;
    if left.parent() != right.parent() {
        sync_parent(right)?;
    }
    Ok(())
}

#[cfg(windows)]
fn durable_exchange(_left: &Path, _right: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "directory exchange is unsupported on Windows",
    ))
}

#[cfg(unix)]
fn directory_identity(path: &Path) -> Result<DirectoryIdentity> {
    use std::os::unix::fs::MetadataExt;

    let metadata = fs::metadata(path).map_err(|_| "update-transaction-bundle-invalid")?;
    if !metadata.is_dir() {
        return Err("update-transaction-bundle-invalid".into());
    }
    Ok(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

impl InstallTransaction {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        source_version: String,
        target_version: String,
        kind: TransactionKind,
        install_path: PathBuf,
        staged_path: PathBuf,
        backup_path: PathBuf,
        files: Vec<PathBuf>,
        was_running: bool,
    ) -> Result<Self> {
        Self::new_with_removed(
            source_version,
            target_version,
            kind,
            install_path,
            staged_path,
            backup_path,
            files,
            Vec::new(),
            was_running,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new_with_removed(
        source_version: String,
        target_version: String,
        kind: TransactionKind,
        install_path: PathBuf,
        staged_path: PathBuf,
        backup_path: PathBuf,
        files: Vec<PathBuf>,
        removed_files: Vec<PathBuf>,
        was_running: bool,
    ) -> Result<Self> {
        if !install_path.is_absolute() || !staged_path.is_absolute() || !backup_path.is_absolute() {
            return Err("update-transaction-invalid".into());
        }
        validate_managed_sibling(&install_path, &staged_path)?;
        validate_managed_sibling(&install_path, &backup_path)?;
        if install_path == staged_path || install_path == backup_path || staged_path == backup_path
        {
            return Err("update-transaction-invalid".into());
        }
        for path in [&install_path, &staged_path, &backup_path] {
            if path.exists() && is_reparse_or_symlink(path)? {
                return Err("update-install-path-linked".into());
            }
        }
        for file in files.iter().chain(&removed_files) {
            validate_relative(file)?;
        }
        if kind == TransactionKind::Files && files.is_empty() && removed_files.is_empty() {
            return Err("update-transaction-invalid".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        if files
            .iter()
            .chain(&removed_files)
            .any(|file| !seen.insert(file.clone()))
        {
            return Err("update-transaction-invalid".into());
        }
        Ok(Self {
            schema: TRANSACTION_SCHEMA.into(),
            source_version,
            target_version,
            kind,
            phase: TransactionPhase::Prepared,
            install_path,
            staged_path,
            backup_path,
            file_states: files
                .into_iter()
                .map(|path| FileTransaction {
                    path,
                    desired_present: true,
                    original_existed: None,
                    phase: FilePhase::Pending,
                })
                .chain(removed_files.into_iter().map(|path| FileTransaction {
                    path,
                    desired_present: false,
                    original_existed: None,
                    phase: FilePhase::Pending,
                }))
                .collect(),
            was_running,
            installer_startup_enabled: None,
            directory_install_identity: None,
            directory_staged_identity: None,
        })
    }

    pub fn load(root: &Path, expected_install: &Path) -> Result<Option<Self>> {
        let path = state_path(root);
        if !path.exists() {
            return Ok(None);
        }
        let bytes = fs::read(&path).map_err(|_| "update-transaction-invalid".to_owned())?;
        let value: Self =
            serde_json::from_slice(&bytes).map_err(|_| "update-transaction-invalid".to_owned())?;
        if value.schema != TRANSACTION_SCHEMA || value.install_path != expected_install {
            return Err("update-transaction-incompatible".into());
        }
        validate_managed_sibling(&value.install_path, &value.staged_path)?;
        validate_managed_sibling(&value.install_path, &value.backup_path)?;
        if value.install_path == value.staged_path
            || value.install_path == value.backup_path
            || value.staged_path == value.backup_path
        {
            return Err("update-transaction-invalid".into());
        }
        for path in [&value.install_path, &value.staged_path, &value.backup_path] {
            if path.exists() && is_reparse_or_symlink(path)? {
                return Err("update-install-path-linked".into());
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for file in &value.file_states {
            validate_relative(&file.path)?;
            if !seen.insert(file.path.clone()) {
                return Err("update-transaction-invalid".into());
            }
            if (file.phase == FilePhase::Pending) != file.original_existed.is_none() {
                return Err("update-transaction-invalid".into());
            }
        }
        if value.kind == TransactionKind::Files
            && (value.directory_install_identity.is_some()
                || value.directory_staged_identity.is_some())
        {
            return Err("update-transaction-invalid".into());
        }
        #[cfg(unix)]
        if value.kind == TransactionKind::Directory {
            let identities_present = value.directory_install_identity.is_some()
                && value.directory_staged_identity.is_some();
            if (value.phase == TransactionPhase::Prepared) == identities_present {
                return Err("update-transaction-invalid".into());
            }
        }
        Ok(Some(value))
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        if self.schema != TRANSACTION_SCHEMA {
            return Err("update-transaction-incompatible".into());
        }
        write_json(&state_path(root), self, "update-transaction-write-failed")
    }

    pub(super) fn set_installer_startup_enabled(&mut self, enabled: bool) {
        self.installer_startup_enabled = Some(enabled);
    }

    pub(super) fn installer_startup_enabled(&self) -> Option<bool> {
        self.installer_startup_enabled
    }

    pub fn preflight_writable(&self) -> Result<()> {
        self.preflight_writable_except(&[])
    }

    pub fn preflight_writable_except(&self, exclusions: &[PathBuf]) -> Result<()> {
        for file in &self.file_states {
            reject_linked_path(&self.install_path, &file.path)?;
            reject_linked_path(&self.staged_path, &file.path)?;
            let target = self.install_path.join(&file.path);
            #[cfg(windows)]
            if target.exists() && !exclusions.iter().any(|path| path == &target) {
                use std::os::windows::fs::OpenOptionsExt;
                OpenOptions::new()
                    .access_mode(0x00010000)
                    .share_mode(0)
                    .open(&target)
                    .map_err(|_| "update-install-locked".to_owned())?;
            }
        }
        Ok(())
    }

    pub fn apply(&mut self, root: &Path) -> Result<()> {
        self.phase = TransactionPhase::Prepared;
        self.save(root)?;
        match self.kind {
            TransactionKind::Directory => self.apply_directory(root),
            TransactionKind::Files => self.apply_files(root),
        }
    }

    fn apply_directory(&mut self, root: &Path) -> Result<()> {
        if self.backup_path.exists() {
            return Err("update-backup-already-exists".into());
        }
        #[cfg(unix)]
        {
            self.directory_install_identity = Some(directory_identity(&self.install_path)?);
            self.directory_staged_identity = Some(directory_identity(&self.staged_path)?);
        }
        self.phase = TransactionPhase::Installing;
        self.save(root)?;
        durable_exchange(&self.install_path, &self.staged_path)
            .map_err(|_| "update-install-replace-failed".to_owned())?;
        self.phase = TransactionPhase::Installed;
        self.save(root)
    }

    fn apply_files(&mut self, root: &Path) -> Result<()> {
        fs::create_dir_all(&self.backup_path)
            .map_err(|_| "update-backup-create-failed".to_owned())?;
        sync_parent(&self.backup_path).map_err(|_| "update-backup-create-failed".to_owned())?;
        self.phase = TransactionPhase::Installing;
        self.save(root)?;
        for index in 0..self.file_states.len() {
            let relative = self.file_states[index].path.clone();
            reject_linked_path(&self.install_path, &relative)?;
            reject_linked_path(&self.staged_path, &relative)?;
            self.file_states[index].original_existed =
                Some(self.install_path.join(&relative).exists());
            self.file_states[index].phase = FilePhase::Prepared;
            self.save(root)?;
            let staged = self.staged_path.join(&relative);
            if self.file_states[index].desired_present && !staged.is_file() {
                return Err("update-package-file-missing".into());
            }
            if !self.file_states[index].desired_present && staged.exists() {
                return Err("update-package-file-unexpected".into());
            }
            let target = self.install_path.join(&relative);
            let backup = self.backup_path.join(&relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|_| "update-install-replace-failed".to_owned())?;
            }
            if let Some(parent) = backup.parent() {
                fs::create_dir_all(parent).map_err(|_| "update-backup-create-failed".to_owned())?;
            }
            if target.exists() && self.file_states[index].desired_present {
                durable_backup_copy(&target, &backup)
                    .map_err(|_| "update-backup-create-failed".to_owned())?;
                durable_replace(&target, &staged)
                    .map_err(|_| "update-install-replace-failed".to_owned())?;
            } else {
                if target.exists() {
                    durable_rename(&target, &backup)
                        .map_err(|_| "update-install-locked".to_owned())?;
                    self.file_states[index].phase = FilePhase::BackedUp;
                    self.save(root)?;
                }
                if self.file_states[index].desired_present {
                    durable_rename(&staged, &target)
                        .map_err(|_| "update-install-replace-failed".to_owned())?;
                }
            }
            self.file_states[index].phase = FilePhase::Installed;
            self.save(root)?;
        }
        self.phase = TransactionPhase::Installed;
        self.save(root)
    }

    pub fn mark_restarting(&mut self, root: &Path) -> Result<()> {
        self.phase = TransactionPhase::Restarting;
        self.save(root)
    }

    pub fn complete(&mut self, root: &Path) -> Result<()> {
        self.phase = TransactionPhase::Completed;
        self.save(root)?;
        let retired = if self.kind == TransactionKind::Directory {
            vec![&self.staged_path]
        } else {
            vec![&self.backup_path, &self.staged_path]
        };
        for retired in retired {
            if retired.exists() {
                let removed = if retired.is_dir() {
                    fs::remove_dir_all(retired)
                } else {
                    fs::remove_file(retired)
                };
                if removed.is_err() || sync_parent(retired).is_err() {
                    return Ok(());
                }
            }
        }
        let state = state_path(root);
        if fs::remove_file(&state).is_ok() {
            let _ = sync_parent(&state);
        }
        Ok(())
    }

    fn discard_rolled_back(&self, root: &Path) -> Result<()> {
        if self.phase != TransactionPhase::RolledBack {
            return Err("update-transaction-invalid".into());
        }
        for path in [&self.staged_path, &self.backup_path] {
            if path.exists() {
                if path.is_dir() {
                    fs::remove_dir_all(path)
                } else {
                    fs::remove_file(path)
                }
                .map_err(|_| "update-staging-cleanup-failed".to_owned())?;
                sync_parent(path).map_err(|_| "update-staging-cleanup-failed".to_owned())?;
            }
        }
        let state = state_path(root);
        fs::remove_file(&state).map_err(|_| "update-transaction-write-failed".to_owned())?;
        sync_parent(&state).map_err(|_| "update-transaction-write-failed".to_owned())
    }

    pub fn cancel_prepared(&self, root: &Path) -> Result<()> {
        if self.phase != TransactionPhase::Prepared {
            return Err("update-transaction-invalid".into());
        }
        for path in [&self.staged_path, &self.backup_path] {
            if path.exists() {
                if path.is_dir() {
                    fs::remove_dir_all(path)
                } else {
                    fs::remove_file(path)
                }
                .map_err(|_| "update-staging-cleanup-failed".to_owned())?;
                sync_parent(path).map_err(|_| "update-staging-cleanup-failed".to_owned())?;
            }
        }
        let state = state_path(root);
        fs::remove_file(&state).map_err(|_| "update-transaction-write-failed".to_owned())?;
        sync_parent(&state).map_err(|_| "update-transaction-write-failed".to_owned())
    }

    pub fn rollback(&mut self, root: &Path) -> Result<()> {
        if self.phase == TransactionPhase::Prepared {
            for path in [&self.staged_path, &self.backup_path] {
                if path.exists() {
                    if is_reparse_or_symlink(path)? {
                        return Err("update-transaction-linked-path".into());
                    }
                    if path.is_dir() {
                        fs::remove_dir_all(path)
                    } else {
                        fs::remove_file(path)
                    }
                    .map_err(|_| "update-staging-cleanup-failed".to_owned())?;
                    sync_parent(path).map_err(|_| "update-staging-cleanup-failed".to_owned())?;
                }
            }
            self.phase = TransactionPhase::RolledBack;
            return self.save(root);
        }
        self.phase = TransactionPhase::RollingBack;
        self.save(root)?;
        match self.kind {
            TransactionKind::Directory => {
                for path in [&self.install_path, &self.staged_path, &self.backup_path] {
                    if path.exists() && is_reparse_or_symlink(path)? {
                        return Err("update-transaction-linked-path".into());
                    }
                }
                #[cfg(unix)]
                {
                    let original_install = self
                        .directory_install_identity
                        .ok_or("update-transaction-invalid")?;
                    let original_staged = self
                        .directory_staged_identity
                        .ok_or("update-transaction-invalid")?;
                    let installed = directory_identity(&self.install_path)?;
                    let staged = directory_identity(&self.staged_path)?;
                    if installed == original_staged && staged == original_install {
                        durable_exchange(&self.install_path, &self.staged_path)
                            .map_err(|_| "update-rollback-failed".to_owned())?;
                    } else if installed != original_install || staged != original_staged {
                        return Err("update-rollback-failed".into());
                    }
                }
                #[cfg(not(unix))]
                return Err("update-rollback-failed".into());
            }
            TransactionKind::Files => {
                for index in (0..self.file_states.len()).rev() {
                    if self.file_states[index].phase == FilePhase::Restored
                        || self.file_states[index].original_existed.is_none()
                    {
                        continue;
                    }
                    let relative = self.file_states[index].path.clone();
                    reject_linked_path(&self.install_path, &relative)?;
                    reject_linked_path(&self.backup_path, &relative)?;
                    let target = self.install_path.join(&relative);
                    let backup = self.backup_path.join(&relative);
                    if self.file_states[index].original_existed == Some(false) {
                        if backup.exists() {
                            if target.exists() {
                                return Err("update-rollback-failed".into());
                            }
                        } else if target.exists() && !self.staged_path.join(&relative).exists() {
                            durable_rename(&target, &backup)
                                .map_err(|_| "update-rollback-failed".to_owned())?;
                        }
                    } else if backup.exists() {
                        if let Some(parent) = target.parent() {
                            fs::create_dir_all(parent)
                                .map_err(|_| "update-rollback-failed".to_owned())?;
                        }
                        if target.exists() {
                            durable_replace(&target, &backup)
                                .map_err(|_| "update-rollback-failed".to_owned())?;
                        } else {
                            durable_rename(&backup, &target)
                                .map_err(|_| "update-rollback-failed".to_owned())?;
                        }
                    } else if self.file_states[index].original_existed == Some(true)
                        && !target.exists()
                    {
                        return Err("update-rollback-failed".into());
                    }
                    self.file_states[index].phase = FilePhase::Restored;
                    self.save(root)?;
                }
            }
        }
        self.phase = TransactionPhase::RolledBack;
        self.save(root)
    }
}

pub fn recover_transaction(
    root: &Path,
    expected_install: &Path,
) -> Result<Option<RecoveryOutcome>> {
    let Some(mut transaction) = InstallTransaction::load(root, expected_install)? else {
        return Ok(None);
    };
    let was_running = transaction.was_running;
    let source_version = transaction.source_version.clone();
    let target_version = transaction.target_version.clone();
    let installer_startup_enabled = transaction.installer_startup_enabled;
    let outcome = match transaction.phase {
        TransactionPhase::Completed => {
            transaction.complete(root)?;
            RecoveryOutcome::Completed { target_version }
        }
        TransactionPhase::RolledBack => RecoveryOutcome::RolledBack {
            was_running,
            source_version,
            target_version,
            installer_startup_enabled,
        },
        _ => {
            transaction.rollback(root)?;
            return Ok(Some(RecoveryOutcome::RolledBack {
                was_running,
                source_version,
                target_version,
                installer_startup_enabled,
            }));
        }
    };
    Ok(Some(outcome))
}

pub fn finish_rollback_recovery(root: &Path, expected_install: &Path) -> Result<()> {
    let transaction =
        InstallTransaction::load(root, expected_install)?.ok_or("update-transaction-missing")?;
    transaction.discard_rolled_back(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &Path, value: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, value).unwrap();
    }

    #[cfg(unix)]
    fn bundle(path: &Path, version: &str) {
        file(&path.join("Contents/version"), version);
        file(
            &path.join("risunest-sync-bundle.json"),
            &serde_json::json!({"version":version}).to_string(),
        );
    }

    #[cfg(unix)]
    #[test]
    fn whole_bundle_can_be_rolled_back_after_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let install = temp.path().join("RisuNest Sync.app");
        let staged = temp.path().join(".risunest-sync-update-staged.app");
        let backup = temp.path().join(".risunest-sync-update-backup.app");
        bundle(&install, "1.0.0");
        bundle(&staged, "2.0.0");
        let mut tx = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Directory,
            install.clone(),
            staged,
            backup,
            Vec::new(),
            true,
        )
        .unwrap();
        tx.apply(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("Contents/version")).unwrap(),
            "2.0.0"
        );
        tx.rollback(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("Contents/version")).unwrap(),
            "1.0.0"
        );
    }

    #[cfg(unix)]
    #[test]
    fn same_version_directory_rollback_uses_identity_before_and_after_exchange() {
        fn fixture() -> (
            tempfile::TempDir,
            PathBuf,
            PathBuf,
            PathBuf,
            PathBuf,
            InstallTransaction,
        ) {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("data");
            let install = temp.path().join("install");
            let staged = temp.path().join(".risunest-sync-update-stage");
            let backup = temp.path().join(".risunest-sync-update-backup");
            bundle(&install, "1.0.0");
            bundle(&staged, "1.0.0");
            file(&install.join("Contents/flavor"), "old");
            file(&staged.join("Contents/flavor"), "new");
            let transaction = InstallTransaction::new(
                "1.0.0".into(),
                "1.0.0".into(),
                TransactionKind::Directory,
                install.clone(),
                staged.clone(),
                backup.clone(),
                Vec::new(),
                true,
            )
            .unwrap();
            (temp, root, install, staged, backup, transaction)
        }

        let (_temp, root, install, staged, _backup, mut before_exchange) = fixture();
        before_exchange.directory_install_identity = Some(directory_identity(&install).unwrap());
        before_exchange.directory_staged_identity = Some(directory_identity(&staged).unwrap());
        before_exchange.phase = TransactionPhase::Installing;
        before_exchange.save(&root).unwrap();
        before_exchange.rollback(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("Contents/flavor")).unwrap(),
            "old"
        );

        let (_temp, root, install, _staged, _backup, mut after_exchange) = fixture();
        after_exchange.apply(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("Contents/flavor")).unwrap(),
            "new"
        );
        after_exchange.rollback(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("Contents/flavor")).unwrap(),
            "old"
        );

        let (_temp, root, install, staged, _backup, mut interrupted_rollback) = fixture();
        interrupted_rollback.apply(&root).unwrap();
        interrupted_rollback.phase = TransactionPhase::RollingBack;
        interrupted_rollback.save(&root).unwrap();
        durable_exchange(&install, &staged).unwrap();
        let mut recovered = InstallTransaction::load(&root, &install).unwrap().unwrap();
        recovered.rollback(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("Contents/flavor")).unwrap(),
            "old"
        );
    }

    #[test]
    fn file_bundle_replaces_only_the_signed_inventory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let install = temp.path().join("install");
        let staged = temp.path().join(".risunest-sync-update-stage");
        let backup = temp.path().join(".risunest-sync-update-backup");
        file(&install.join("server.exe"), "old");
        file(&install.join("uninstall.exe"), "installer-owned");
        file(&staged.join("server.exe"), "new");
        let mut tx = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Files,
            install.clone(),
            staged,
            backup,
            vec![PathBuf::from("server.exe")],
            true,
        )
        .unwrap();
        tx.apply(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("server.exe")).unwrap(),
            "new"
        );
        assert_eq!(
            fs::read_to_string(install.join("uninstall.exe")).unwrap(),
            "installer-owned"
        );
        tx.rollback(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("server.exe")).unwrap(),
            "old"
        );
        tx.rollback(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("server.exe")).unwrap(),
            "old"
        );
    }

    #[test]
    fn file_bundle_removes_old_owned_files_but_can_restore_them() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let install = temp.path().join("managed-install");
        let staged = temp.path().join(".risunest-sync-update-stage");
        let backup = temp.path().join(".risunest-sync-update-backup");
        file(&install.join("server.exe"), "old");
        file(&install.join("obsolete.dll"), "old-owned");
        file(&install.join("uninstall.exe"), "installer-owned");
        file(&staged.join("server.exe"), "new");
        let mut transaction = InstallTransaction::new_with_removed(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Files,
            install.clone(),
            staged,
            backup,
            vec![PathBuf::from("server.exe")],
            vec![PathBuf::from("obsolete.dll")],
            true,
        )
        .unwrap();
        transaction.apply(&root).unwrap();
        assert!(!install.join("obsolete.dll").exists());
        assert_eq!(
            fs::read_to_string(install.join("uninstall.exe")).unwrap(),
            "installer-owned"
        );
        transaction.rollback(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("obsolete.dll")).unwrap(),
            "old-owned"
        );
    }

    #[test]
    fn rollback_of_a_new_file_is_retry_safe_after_its_durable_retirement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let install = temp.path().join("install");
        let staged = temp.path().join(".risunest-sync-update-stage");
        let backup = temp.path().join(".risunest-sync-update-backup");
        file(&staged.join("added.dll"), "new");
        let mut transaction = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Files,
            install.clone(),
            staged,
            backup.clone(),
            vec![PathBuf::from("added.dll")],
            true,
        )
        .unwrap();
        transaction.apply(&root).unwrap();
        fs::create_dir_all(&backup).unwrap();
        durable_rename(&install.join("added.dll"), &backup.join("added.dll")).unwrap();
        transaction.rollback(&root).unwrap();
        assert!(!install.join("added.dll").exists());
        transaction.rollback(&root).unwrap();
        assert!(!install.join("added.dll").exists());
    }

    #[test]
    fn rollback_before_backup_never_deletes_the_original() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let install = temp.path().join("install");
        let staged = temp.path().join(".risunest-sync-update-stage");
        let backup = temp.path().join(".risunest-sync-update-backup");
        file(&install.join("server.exe"), "old");
        file(&staged.join("server.exe"), "new");
        let mut tx = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Files,
            install.clone(),
            staged,
            backup,
            vec![PathBuf::from("server.exe")],
            true,
        )
        .unwrap();
        tx.file_states[0].original_existed = Some(true);
        tx.file_states[0].phase = FilePhase::Prepared;
        tx.save(&root).unwrap();
        tx.rollback(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("server.exe")).unwrap(),
            "old"
        );
    }

    #[test]
    fn incomplete_backup_copy_is_never_published_or_restored() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let install = temp.path().join("install");
        let staged = temp.path().join(".risunest-sync-update-stage");
        let backup = temp.path().join(".risunest-sync-update-backup");
        file(&install.join("server.exe"), "complete-old");
        file(&staged.join("server.exe"), "new");
        file(&backup.join("server.exe.pending"), "truncated");
        let mut transaction = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Files,
            install.clone(),
            staged,
            backup.clone(),
            vec![PathBuf::from("server.exe")],
            true,
        )
        .unwrap();
        assert_eq!(
            transaction.apply(&root).unwrap_err(),
            "update-backup-create-failed"
        );
        assert!(!backup.join("server.exe").exists());
        transaction.rollback(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("server.exe")).unwrap(),
            "complete-old"
        );
    }

    #[test]
    fn rollback_after_atomic_replacement_restores_once_and_is_retry_safe() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let install = temp.path().join("install");
        let staged = temp.path().join(".risunest-sync-update-stage");
        let backup = temp.path().join(".risunest-sync-update-backup");
        file(&install.join("server.exe"), "old");
        file(&staged.join("server.exe"), "new");
        let mut tx = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Files,
            install.clone(),
            staged,
            backup.clone(),
            vec![PathBuf::from("server.exe")],
            true,
        )
        .unwrap();
        tx.file_states[0].original_existed = Some(true);
        tx.file_states[0].phase = FilePhase::Prepared;
        tx.phase = TransactionPhase::Installing;
        fs::create_dir_all(&backup).unwrap();
        fs::copy(install.join("server.exe"), backup.join("server.exe")).unwrap();
        fs::write(install.join("server.exe"), "new").unwrap();
        tx.save(&root).unwrap();
        tx.rollback(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("server.exe")).unwrap(),
            "old"
        );
        tx.rollback(&root).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("server.exe")).unwrap(),
            "old"
        );
    }

    #[test]
    fn recovery_rejects_a_transaction_for_another_install() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let tx = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Directory,
            temp.path().join("one"),
            temp.path().join(".risunest-sync-update-stage"),
            temp.path().join(".risunest-sync-update-backup"),
            Vec::new(),
            false,
        )
        .unwrap();
        tx.save(&root).unwrap();
        assert_eq!(
            InstallTransaction::load(&root, &temp.path().join("two")).unwrap_err(),
            "update-transaction-incompatible"
        );
    }

    #[test]
    fn prepared_directory_recovery_does_not_require_the_unpublished_stage() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let install = temp.path().join("install");
        let staged = temp.path().join(".risunest-sync-update-stage");
        let backup = temp.path().join(".risunest-sync-update-backup");
        file(&install.join("server"), "old");
        let mut transaction = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Directory,
            install.clone(),
            staged,
            backup,
            Vec::new(),
            true,
        )
        .unwrap();
        transaction.set_installer_startup_enabled(false);
        transaction.save(&root).unwrap();
        assert_eq!(
            recover_transaction(&root, &install).unwrap(),
            Some(RecoveryOutcome::RolledBack {
                was_running: true,
                source_version: "1.0.0".into(),
                target_version: "2.0.0".into(),
                installer_startup_enabled: Some(false),
            })
        );
        assert_eq!(fs::read_to_string(install.join("server")).unwrap(), "old");
    }

    #[cfg(unix)]
    #[test]
    fn recovery_of_an_installed_bundle_restores_old_directory() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let install = temp.path().join("install");
        let staged = temp.path().join(".risunest-sync-update-stage");
        let backup = temp.path().join(".risunest-sync-update-backup");
        bundle(&install, "1.0.0");
        bundle(&staged, "2.0.0");
        let mut tx = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Directory,
            install.clone(),
            staged,
            backup,
            Vec::new(),
            true,
        )
        .unwrap();
        tx.apply(&root).unwrap();
        assert_eq!(
            recover_transaction(&root, &install).unwrap(),
            Some(RecoveryOutcome::RolledBack {
                was_running: true,
                source_version: "1.0.0".into(),
                target_version: "2.0.0".into(),
                installer_startup_enabled: None,
            })
        );
        assert_eq!(
            fs::read_to_string(install.join("Contents/version")).unwrap(),
            "1.0.0"
        );
        assert_eq!(
            InstallTransaction::load(&root, &install)
                .unwrap()
                .unwrap()
                .phase,
            TransactionPhase::RolledBack
        );
        finish_rollback_recovery(&root, &install).unwrap();
        assert!(InstallTransaction::load(&root, &install).unwrap().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn live_preflight_ignores_owned_processes_but_defers_for_an_open_gui() {
        use std::os::windows::fs::OpenOptionsExt;

        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("managed-install");
        let staged = temp.path().join(".risunest-sync-update-stage");
        let backup = temp.path().join(".risunest-sync-update-backup");
        for name in ["risunest-sync-server.exe", "risunest-sync-gui.exe"] {
            file(&install.join(name), "old");
            file(&staged.join(name), "new");
        }
        let transaction = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Files,
            install.clone(),
            staged,
            backup,
            vec![
                PathBuf::from("risunest-sync-server.exe"),
                PathBuf::from("risunest-sync-gui.exe"),
            ],
            true,
        )
        .unwrap();
        let server_path = install.join("risunest-sync-server.exe");
        let _server_handle = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&server_path)
            .unwrap();
        transaction
            .preflight_writable_except(std::slice::from_ref(&server_path))
            .unwrap();

        let _gui_handle = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(install.join("risunest-sync-gui.exe"))
            .unwrap();
        assert_eq!(
            transaction
                .preflight_writable_except(std::slice::from_ref(&server_path))
                .unwrap_err(),
            "update-install-locked"
        );
    }
}
