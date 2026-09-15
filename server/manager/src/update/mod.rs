use crate::{platform, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

mod archive;
mod transaction;
pub use archive::{
    extract_package, managed_bundle_version, validate_installed_marker, ExtractedBundle,
};
mod engine;
pub use engine::{run, run_helper, run_recovery_helper, run_while_locked, RunMode, RunOutcome};
pub use transaction::{
    finish_rollback_recovery, recover_transaction, InstallTransaction, RecoveryOutcome,
    TransactionKind, TransactionPhase,
};

const SETTINGS_SCHEMA: &str = "risunest-sync-update-settings/v1";
const STATUS_SCHEMA: &str = "risunest-sync-update-status/v1";
const INSTALLER_GUARD_POLL: Duration = Duration::from_millis(100);
const INSTALLER_GUARD_PREPARE_TIMEOUT: Duration = Duration::from_secs(45);
const INSTALLER_GUARD_RELEASE_TIMEOUT: Duration = Duration::from_secs(30);
const INSTALLER_GUARD_ABANDON_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdatePolicy {
    #[default]
    Automatic,
    Notify,
    Off,
}

impl UpdatePolicy {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "automatic" => Ok(Self::Automatic),
            "notify" => Ok(Self::Notify),
            "off" => Ok(Self::Off),
            _ => Err("invalid-update-policy".into()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettings {
    pub schema: String,
    pub policy: UpdatePolicy,
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            schema: SETTINGS_SCHEMA.into(),
            policy: UpdatePolicy::Automatic,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdatePhase {
    #[default]
    Idle,
    Checking,
    Downloading,
    WaitingIdle,
    Draining,
    Installing,
    Restarting,
    Completed,
    Deferred,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateStatus {
    pub schema: String,
    pub phase: UpdatePhase,
    pub target_version: Option<String>,
    pub last_checked_at: Option<u64>,
    pub last_completed_at: Option<u64>,
    pub deferred_until: Option<u64>,
    pub reason: Option<String>,
    pub last_failed_version: Option<String>,
}

impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            schema: STATUS_SCHEMA.into(),
            phase: UpdatePhase::Idle,
            target_version: None,
            last_checked_at: None,
            last_completed_at: None,
            deferred_until: None,
            reason: None,
            last_failed_version: None,
        }
    }
}

#[derive(Debug)]
pub struct UpdateLock {
    _file: File,
}

pub(super) struct HelperLock {
    _file: File,
}

pub struct ActivityGuard {
    file: File,
    path: PathBuf,
}

fn directory(root: &Path) -> PathBuf {
    root.join("manager-update")
}

fn read_json<T: for<'a> Deserialize<'a>>(path: &Path, invalid: &'static str) -> Result<T> {
    let bytes = fs::read(path).map_err(|_| invalid.to_owned())?;
    serde_json::from_slice(&bytes).map_err(|_| invalid.to_owned())
}

#[cfg(windows)]
fn persist_json(file: tempfile::NamedTempFile, path: &Path, error: &'static str) -> Result<()> {
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
    let (handle, temporary) = file.into_parts();
    drop(handle);
    let source = wide(&temporary);
    let target = wide(path);
    let flags = MOVEFILE_WRITE_THROUGH
        | if path.exists() {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    let succeeded = unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), flags) };
    if succeeded == 0 {
        return Err(error.into());
    }
    Ok(())
}

pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T, error: &'static str) -> Result<()> {
    let parent = path.parent().ok_or(error)?;
    fs::create_dir_all(parent).map_err(|_| error.to_owned())?;
    let bytes = serde_json::to_vec(value).map_err(|_| error.to_owned())?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| error.to_owned())?;
    file.write_all(&bytes).map_err(|_| error.to_owned())?;
    file.as_file().sync_all().map_err(|_| error.to_owned())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| error.to_owned())?;
    }
    #[cfg(windows)]
    persist_json(file, path, error)?;
    #[cfg(not(windows))]
    file.persist(path).map_err(|_| error.to_owned())?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| error.to_owned())?;
    Ok(())
}

pub fn load_settings(root: &Path) -> Result<UpdateSettings> {
    let path = directory(root).join("settings.json");
    if !path.exists() {
        return Ok(UpdateSettings::default());
    }
    let value: UpdateSettings = read_json(&path, "update-settings-invalid")?;
    if value.schema != SETTINGS_SCHEMA {
        return Err("update-settings-incompatible".into());
    }
    Ok(value)
}

pub fn save_policy(root: &Path, policy: UpdatePolicy) -> Result<UpdateSettings> {
    let value = UpdateSettings {
        policy,
        ..UpdateSettings::default()
    };
    write_json(
        &directory(root).join("settings.json"),
        &value,
        "update-settings-write-failed",
    )?;
    Ok(value)
}

pub fn set_policy(
    root: &Path,
    manager: &Path,
    server: &Path,
    policy: UpdatePolicy,
) -> Result<UpdateSettings> {
    let _lock = try_lock(root)?;
    set_policy_while_locked(root, manager, server, policy)
}

pub fn set_policy_while_locked(
    root: &Path,
    manager: &Path,
    server: &Path,
    policy: UpdatePolicy,
) -> Result<UpdateSettings> {
    if directory(root).join("transaction.json").exists() {
        return Err("update-recovery-required".into());
    }
    set_policy_locked(root, manager, server, policy)
}

pub fn installer_set_policy_while_locked(
    root: &Path,
    manager: &Path,
    server: &Path,
    policy: UpdatePolicy,
) -> Result<UpdateSettings> {
    validate_installer_configuration(root, server)?;
    set_policy_locked(root, manager, server, policy)
}

fn set_policy_locked(
    root: &Path,
    manager: &Path,
    server: &Path,
    policy: UpdatePolicy,
) -> Result<UpdateSettings> {
    let previous = load_settings(root)?;
    let previous_schedule =
        platform::update_schedule(root, manager, server, previous.policy, "status")?;
    let restore_schedule = || -> Result<()> {
        if previous_schedule.registered
            && previous_schedule.enabled
            && previous_schedule.action_matches
        {
            platform::update_schedule(root, manager, server, previous.policy, "install").map(|_| ())
        } else if !previous_schedule.registered {
            platform::update_schedule(root, manager, server, previous.policy, "remove").map(|_| ())
        } else {
            Err("update-policy-rollback-failed".into())
        }
    };
    if let Err(error) = platform::update_schedule(root, manager, server, policy, "install") {
        return if restore_schedule().is_ok() {
            Err(error)
        } else {
            Err("update-policy-rollback-failed".into())
        };
    }
    match save_policy(root, policy) {
        Ok(value) => Ok(value),
        Err(error) if restore_schedule().is_ok() => Err(error),
        Err(_) => Err("update-policy-rollback-failed".into()),
    }
}

pub fn install_schedule(root: &Path, manager: &Path, server: &Path) -> Result<()> {
    let _lock = try_lock(root)?;
    reconcile_schedule_while_locked(root, manager, server)
}

pub fn install_schedule_while_locked(root: &Path, manager: &Path, server: &Path) -> Result<()> {
    let policy = load_settings(root)?.policy;
    platform::update_schedule(root, manager, server, policy, "install").map(|_| ())
}

pub fn reconcile_schedule(root: &Path, manager: &Path, server: &Path) -> Result<()> {
    let _lock = try_lock(root)?;
    reconcile_schedule_while_locked(root, manager, server)
}

pub fn reconcile_schedule_while_locked(root: &Path, manager: &Path, server: &Path) -> Result<()> {
    if directory(root).join("transaction.json").exists() {
        return Err("update-recovery-required".into());
    }
    let policy = load_settings(root)?.policy;
    let status = platform::update_schedule(root, manager, server, policy, "status")?;
    let matches_policy = if policy == UpdatePolicy::Off {
        !status.registered
    } else {
        status.registered && status.enabled && status.action_matches
    };
    if matches_policy {
        Ok(())
    } else {
        platform::update_schedule(root, manager, server, policy, "install").map(|_| ())
    }
}

pub fn remove_schedule(root: &Path, manager: &Path, server: &Path) -> Result<()> {
    let _lock = try_lock(root)?;
    remove_schedule_while_locked(root, manager, server)
}

pub fn remove_schedule_while_locked(root: &Path, manager: &Path, server: &Path) -> Result<()> {
    let policy = load_settings(root)?.policy;
    platform::update_schedule(root, manager, server, policy, "remove").map(|_| ())
}

pub fn set_autostart(
    root: &Path,
    manager: &Path,
    server: &Path,
    enabled: bool,
) -> Result<platform::StartupStatus> {
    let _lock = try_lock(root)?;
    set_autostart_while_locked(root, manager, server, enabled)
}

pub fn set_autostart_while_locked(
    root: &Path,
    _manager: &Path,
    server: &Path,
    enabled: bool,
) -> Result<platform::StartupStatus> {
    if directory(root).join("transaction.json").exists() {
        return Err("update-recovery-required".into());
    }
    set_autostart_locked(root, server, enabled)
}

pub fn installer_set_autostart_while_locked(
    root: &Path,
    _manager: &Path,
    server: &Path,
    enabled: bool,
) -> Result<platform::StartupStatus> {
    validate_installer_configuration(root, server)?;
    set_autostart_locked(root, server, enabled)
}

fn set_autostart_locked(
    root: &Path,
    server: &Path,
    enabled: bool,
) -> Result<platform::StartupStatus> {
    let prior = platform::startup(root, server, "status")?;
    let restore_startup = || -> Result<()> {
        if prior.registered && prior.enabled && prior.action_matches {
            platform::startup(root, server, "install").map(|_| ())
        } else if !prior.registered {
            platform::startup(root, server, "remove").map(|_| ())
        } else {
            Err("autostart-rollback-failed".into())
        }
    };
    let startup_action = if enabled { "install" } else { "remove" };
    match platform::startup(root, server, startup_action) {
        Ok(status) => Ok(status),
        Err(error) => {
            let startup_restored = restore_startup().is_ok();
            if !startup_restored {
                Err("autostart-rollback-failed".into())
            } else {
                Err(error)
            }
        }
    }
}

fn validate_installer_configuration(root: &Path, server: &Path) -> Result<()> {
    let install = engine::install_path(server)?;
    match InstallTransaction::load(root, &install)? {
        None => Ok(()),
        Some(transaction)
            if transaction.kind == TransactionKind::Directory
                && matches!(
                    transaction.phase,
                    TransactionPhase::Installed | TransactionPhase::Restarting
                ) =>
        {
            Ok(())
        }
        Some(_) => Err("update-recovery-required".into()),
    }
}

fn installer_startup_snapshot(root: &Path, server: &Path) -> Result<bool> {
    let status = platform::startup(root, server, "status")?;
    if status.registered && status.enabled && status.action_matches {
        Ok(true)
    } else if !status.registered {
        Ok(false)
    } else {
        Err("installer-startup-state-unverifiable".into())
    }
}

fn restore_installer_startup(root: &Path, server: &Path, enabled: bool) -> Result<()> {
    let action = if enabled { "install" } else { "remove" };
    let attempted = platform::startup(root, server, action);
    let status = match attempted {
        Ok(status) => status,
        Err(_) => platform::startup(root, server, "status")?,
    };
    let restored = if enabled {
        status.registered && status.enabled && status.action_matches
    } else {
        !status.registered
    };
    if restored {
        Ok(())
    } else {
        Err("installer-startup-restore-failed".into())
    }
}

pub(super) async fn restore_installer_state(
    root: &Path,
    server: &Path,
    was_running: bool,
    startup_enabled: bool,
    version: &str,
) -> Result<()> {
    if !was_running {
        return restore_installer_startup(root, server, startup_enabled);
    }

    if let Err(error) = restore_installer_startup(root, server, true) {
        if !startup_enabled {
            let _ = restore_installer_startup(root, server, false);
        }
        return Err(error);
    }
    let health = engine::start_and_verify(root, server, version).await;
    let startup = if startup_enabled {
        Ok(())
    } else {
        restore_installer_startup(root, server, false)
    };
    if health.is_err() || startup.is_err() {
        Err("installer-state-restore-failed".into())
    } else {
        Ok(())
    }
}

pub fn installer_swap_while_locked(
    root: &Path,
    server: &Path,
    staged: &Path,
    was_running: bool,
) -> Result<()> {
    if !staged.is_absolute() {
        return Err("managed-install-path-invalid".into());
    }
    let install = engine::install_path(server)?;
    if InstallTransaction::load(root, &install)?.is_some() {
        return Err("update-recovery-required".into());
    }
    let source_version = managed_bundle_version(&install)?;
    let target_version = managed_bundle_version(staged)?;
    let prior_startup_enabled = installer_startup_snapshot(root, server)?;
    archive::sync_bundle_tree(staged)?;
    let backup = install
        .parent()
        .ok_or("managed-install-path-invalid")?
        .join(format!(
            ".risunest-sync-update-installer-backup-{}",
            std::process::id()
        ));
    let mut transaction = InstallTransaction::new(
        source_version,
        target_version,
        TransactionKind::Directory,
        install,
        staged.to_owned(),
        backup,
        Vec::new(),
        was_running,
    )?;
    transaction.set_installer_startup_enabled(prior_startup_enabled);
    transaction.save(root)?;
    transaction.apply(root)
}

pub fn installer_sync_stage_while_locked(staged: &Path) -> Result<()> {
    if !staged.is_absolute() {
        return Err("managed-install-path-invalid".into());
    }
    managed_bundle_version(staged)?;
    archive::sync_bundle_tree(staged)
}

pub fn installer_commit_swap_while_locked(root: &Path, server: &Path) -> Result<()> {
    let install = engine::install_path(server)?;
    let mut transaction =
        InstallTransaction::load(root, &install)?.ok_or("update-transaction-missing")?;
    if transaction.kind != TransactionKind::Directory
        || !matches!(
            transaction.phase,
            TransactionPhase::Installed | TransactionPhase::Restarting
        )
    {
        return Err("update-transaction-invalid".into());
    }
    transaction.complete(root)
}

pub async fn installer_rollback_swap_while_locked(
    root: &Path,
    server: &Path,
    fallback_was_running: bool,
) -> Result<()> {
    let install = engine::install_path(server)?;
    let Some(mut transaction) = InstallTransaction::load(root, &install)? else {
        if fallback_was_running {
            let version = managed_bundle_version(&install)?;
            engine::start_and_verify(root, server, &version).await?;
        }
        return Ok(());
    };
    if transaction.kind != TransactionKind::Directory {
        return Err("update-transaction-invalid".into());
    }
    let client = crate::client::Client::new(root.to_owned())?;
    crate::lifecycle::stop(root, &client).await?;
    transaction.rollback(root)?;
    if let Some(enabled) = transaction.installer_startup_enabled() {
        restore_installer_state(
            root,
            server,
            transaction.was_running,
            enabled,
            &transaction.source_version,
        )
        .await?;
    } else if transaction.was_running {
        engine::start_and_verify(root, server, &transaction.source_version).await?;
    }
    finish_rollback_recovery(root, &install)
}

pub async fn installer_start_and_verify_while_locked(root: &Path, server: &Path) -> Result<()> {
    let install = engine::install_path(server)?;
    let version = match InstallTransaction::load(root, &install)? {
        Some(mut transaction)
            if transaction.kind == TransactionKind::Directory
                && matches!(
                    transaction.phase,
                    TransactionPhase::Installed | TransactionPhase::Restarting
                ) =>
        {
            let version = transaction.target_version.clone();
            transaction.mark_restarting(root)?;
            version
        }
        Some(_) => return Err("update-recovery-required".into()),
        None => managed_bundle_version(&install)?,
    };
    engine::start_and_verify(root, server, &version).await
}

fn installer_guard_path(root: &Path, nonce: &str, suffix: &str) -> Result<PathBuf> {
    if nonce.len() != 64
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("installer-guard-invalid".into());
    }
    Ok(directory(root).join(format!("installer-{nonce}.{suffix}")))
}

fn installer_guard_failure<F>(
    ready: &Path,
    error_path: &Path,
    was_running: bool,
    error: &str,
    restart: F,
) -> String
where
    F: FnOnce() -> Result<()>,
{
    let _ = fs::remove_file(ready);
    let restart_failed = restart().is_err();
    let _ = write_json(
        error_path,
        &serde_json::json!({
            "error": error,
            "wasRunning": was_running,
            "restartFailed": restart_failed,
        }),
        "installer-guard-write-failed",
    );
    if restart_failed {
        "installer-guard-restart-failed".into()
    } else {
        error.into()
    }
}

async fn verified_installer_guard_failure(
    root: &Path,
    server: &Path,
    ready: &Path,
    error_path: &Path,
    was_running: bool,
    prior_startup_enabled: bool,
    error: &str,
) -> String {
    let restore =
        match engine::install_path(server).and_then(|install| managed_bundle_version(&install)) {
            Ok(version) => {
                restore_installer_state(root, server, was_running, prior_startup_enabled, &version)
                    .await
            }
            Err(error) => Err(error),
        };
    installer_guard_failure(ready, error_path, was_running, error, || {
        restore.map_err(|_| "installer-guard-restore-failed".to_owned())
    })
}

pub fn begin_installer_guard(root: &Path, server: &Path) -> Result<String> {
    let nonce = risunest_sync_server::management::discovery::request_id()
        .map_err(|_| "installer-guard-unavailable".to_owned())?;
    let ready = installer_guard_path(root, &nonce, "ready")?;
    let error = installer_guard_path(root, &nonce, "error")?;
    let release = installer_guard_path(root, &nonce, "release")?;
    let cancel = installer_guard_path(root, &nonce, "cancel")?;
    for path in [&ready, &error, &release, &cancel] {
        if path.exists() {
            return Err("installer-guard-unavailable".into());
        }
    }
    let helper = engine::copy_helper(root)?;
    let mut command = Command::new(helper);
    command
        .args(["--data-dir"])
        .arg(root)
        .args(["--server"])
        .arg(server)
        .args(["installer", "guard", &nonce])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    platform::process(command.get_program())
        .args(command.get_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "installer-guard-unavailable".to_owned())?;
    let deadline = std::time::Instant::now() + INSTALLER_GUARD_PREPARE_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if ready.is_file() {
            return Ok(nonce);
        }
        if error.is_file() {
            if read_json::<serde_json::Value>(&error, "installer-guard-prepare-failed")
                .ok()
                .and_then(|value| value["restartFailed"].as_bool())
                == Some(true)
            {
                return Err("installer-guard-restart-failed".into());
            }
            return Err("installer-guard-prepare-failed".into());
        }
        thread::sleep(INSTALLER_GUARD_POLL);
    }
    let _ = write_json(
        &cancel,
        &serde_json::json!({"cancel":true}),
        "installer-guard-release-failed",
    );
    Err("installer-guard-timeout".into())
}

pub async fn run_installer_guard(root: &Path, server: &Path, nonce: &str) -> Result<()> {
    let ready = installer_guard_path(root, nonce, "ready")?;
    let error_path = installer_guard_path(root, nonce, "error")?;
    let release = installer_guard_path(root, nonce, "release")?;
    let cancel = installer_guard_path(root, nonce, "cancel")?;
    let result = async {
        let _lock = try_lock(root)?;
        let mut was_running =
            crate::lifecycle::owner_active(root)? || crate::lifecycle::locator_exists(root)?;
        let mut prior_startup_enabled = installer_startup_snapshot(root, server)?;
        if cancel.is_file() {
            let _ = fs::remove_file(&cancel);
            return Err("installer-guard-cancelled".into());
        }
        let client = crate::client::Client::new(root.to_owned())?;
        let prepare_result = async {
            crate::lifecycle::stop(root, &client).await?;
            let install = engine::install_path(server)?;
            if let Some(RecoveryOutcome::RolledBack {
                was_running: recovered_was_running,
                source_version,
                installer_startup_enabled,
                ..
            }) = recover_transaction(root, &install)?
            {
                was_running = recovered_was_running;
                if let Some(enabled) = installer_startup_enabled {
                    prior_startup_enabled = enabled;
                    restore_installer_state(
                        root,
                        server,
                        recovered_was_running,
                        enabled,
                        &source_version,
                    )
                    .await?;
                } else if recovered_was_running {
                    engine::start_and_verify(root, server, &source_version).await?;
                }
                if recovered_was_running {
                    crate::lifecycle::stop(root, &client).await?;
                }
                finish_rollback_recovery(root, &install)?;
            }
            if cancel.is_file() {
                let _ = fs::remove_file(&cancel);
                return Err("installer-guard-cancelled".into());
            }
            write_json(
                &ready,
                &serde_json::json!({"ready":true,"wasRunning":was_running}),
                "installer-guard-write-failed",
            )
        }
        .await;
        if let Err(error) = prepare_result {
            return Err(verified_installer_guard_failure(
                root,
                server,
                &ready,
                &error_path,
                was_running,
                prior_startup_enabled,
                &error,
            )
            .await);
        }
        let deadline = tokio::time::Instant::now() + INSTALLER_GUARD_ABANDON_TIMEOUT;
        while tokio::time::Instant::now() < deadline {
            if cancel.is_file() {
                let _ = fs::remove_file(&cancel);
                return Err(verified_installer_guard_failure(
                    root,
                    server,
                    &ready,
                    &error_path,
                    was_running,
                    prior_startup_enabled,
                    "installer-guard-cancelled",
                )
                .await);
            }
            if release.is_file() {
                fs::remove_file(&release)
                    .map_err(|_| "installer-guard-release-failed".to_owned())?;
                fs::remove_file(&ready).map_err(|_| "installer-guard-release-failed".to_owned())?;
                return Ok(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(verified_installer_guard_failure(
            root,
            server,
            &ready,
            &error_path,
            was_running,
            prior_startup_enabled,
            "installer-guard-timeout",
        )
        .await)
    }
    .await;
    if result.is_err() && !ready.exists() && !error_path.exists() {
        let _ = write_json(
            &error_path,
            &serde_json::json!({"error":"installer-guard-prepare-failed"}),
            "installer-guard-write-failed",
        );
    }
    result
}

pub fn finish_installer_guard(root: &Path, nonce: &str) -> Result<()> {
    let ready = installer_guard_path(root, nonce, "ready")?;
    if !ready.is_file() {
        return Err("installer-guard-not-active".into());
    }
    let release = installer_guard_path(root, nonce, "release")?;
    write_json(
        &release,
        &serde_json::json!({"release":true}),
        "installer-guard-release-failed",
    )?;
    let deadline = std::time::Instant::now() + INSTALLER_GUARD_RELEASE_TIMEOUT;
    while std::time::Instant::now() < deadline {
        if !ready.exists() {
            return Ok(());
        }
        thread::sleep(INSTALLER_GUARD_POLL);
    }
    Err("installer-guard-release-timeout".into())
}

pub fn load_status(root: &Path) -> Result<UpdateStatus> {
    let path = directory(root).join("status.json");
    if !path.exists() {
        return Ok(UpdateStatus::default());
    }
    let value: UpdateStatus = read_json(&path, "update-status-invalid")?;
    if value.schema != STATUS_SCHEMA {
        return Err("update-status-incompatible".into());
    }
    Ok(value)
}

pub fn save_status(root: &Path, status: &UpdateStatus) -> Result<()> {
    if status.schema != STATUS_SCHEMA {
        return Err("update-status-incompatible".into());
    }
    write_json(
        &directory(root).join("status.json"),
        status,
        "update-status-write-failed",
    )
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub fn try_lock(root: &Path) -> Result<UpdateLock> {
    let lock = try_lock_for_helper(root)?;
    if helper_active(root)? {
        return Err("update-already-running".into());
    }
    Ok(lock)
}

pub(super) fn try_lock_for_helper(root: &Path) -> Result<UpdateLock> {
    let directory = directory(root);
    fs::create_dir_all(&directory).map_err(|_| "update-lock-unavailable".to_owned())?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("instance.lock"))
        .map_err(|_| "update-lock-unavailable".to_owned())?;
    file.try_lock_exclusive()
        .map_err(|_| "update-already-running".to_owned())?;
    Ok(UpdateLock { _file: file })
}

pub(super) fn try_helper_lock(root: &Path) -> Result<HelperLock> {
    let directory = directory(root);
    fs::create_dir_all(&directory).map_err(|_| "update-lock-unavailable".to_owned())?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("helper.lock"))
        .map_err(|_| "update-lock-unavailable".to_owned())?;
    file.try_lock_exclusive()
        .map_err(|_| "update-already-running".to_owned())?;
    Ok(HelperLock { _file: file })
}

pub(super) fn helper_active(root: &Path) -> Result<bool> {
    let directory = directory(root);
    fs::create_dir_all(&directory).map_err(|_| "update-lock-unavailable".to_owned())?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("helper.lock"))
        .map_err(|_| "update-lock-unavailable".to_owned())?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(false)
        }
        Err(_) => Ok(true),
    }
}

fn activity_path(root: &Path, process_id: u32) -> PathBuf {
    directory(root).join(format!("activity-{process_id}.lease"))
}

pub fn set_activity(root: &Path, process_id: u32, active: bool) -> Result<()> {
    let path = activity_path(root, process_id);
    if !active {
        if path.exists() {
            fs::remove_file(path).map_err(|_| "update-activity-write-failed".to_owned())?;
        }
        return Ok(());
    }
    write_json(
        &path,
        &serde_json::json!({"expiresAt":now() + 45}),
        "update-activity-write-failed",
    )
}

pub fn has_active_activity(root: &Path) -> Result<bool> {
    let path = directory(root);
    if !path.exists() {
        return Ok(false);
    }
    let current = now();
    for entry in fs::read_dir(path).map_err(|_| "update-activity-read-failed".to_owned())? {
        let entry = entry.map_err(|_| "update-activity-read-failed".to_owned())?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("activity-") || !name.ends_with(".lease") {
            continue;
        }
        let entry_path = entry.path();
        let file = OpenOptions::new().read(true).write(true).open(&entry_path);
        let Ok(mut file) = file else {
            return Ok(true);
        };
        if file.try_lock_exclusive().is_err() {
            return Ok(true);
        }
        let mut bytes = Vec::new();
        let expires = file
            .rewind()
            .and_then(|_| file.read_to_end(&mut bytes))
            .ok()
            .map(|_| bytes)
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .and_then(|value| value["expiresAt"].as_u64())
            .unwrap_or(0);
        let _ = FileExt::unlock(&file);
        if expires > current {
            return Ok(true);
        }
        let _ = fs::remove_file(entry_path);
    }
    Ok(false)
}

impl ActivityGuard {
    pub fn start(root: &Path) -> Result<Self> {
        let process_id = std::process::id();
        let path = activity_path(root, process_id);
        set_activity(root, process_id, true)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|_| "update-activity-write-failed".to_owned())?;
        file.try_lock_exclusive()
            .map_err(|_| "update-activity-write-failed".to_owned())?;
        Ok(Self { file, path })
    }
}

impl Drop for ActivityGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_is_the_default_without_creating_sync_data() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            load_settings(root.path()).unwrap().policy,
            UpdatePolicy::Automatic
        );
        assert!(!root.path().join("manager-update").exists());
    }

    #[test]
    fn policy_and_status_round_trip_in_manager_local_storage() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            save_policy(root.path(), UpdatePolicy::Notify)
                .unwrap()
                .policy,
            UpdatePolicy::Notify
        );
        assert_eq!(
            load_settings(root.path()).unwrap().policy,
            UpdatePolicy::Notify
        );
        let status = UpdateStatus {
            phase: UpdatePhase::Deferred,
            target_version: Some("2.0.0".into()),
            deferred_until: Some(42),
            reason: Some("server-busy".into()),
            ..UpdateStatus::default()
        };
        save_status(root.path(), &status).unwrap();
        assert_eq!(load_status(root.path()).unwrap(), status);
    }

    #[test]
    fn invalid_state_is_reported_instead_of_reset() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("manager-update")).unwrap();
        fs::write(root.path().join("manager-update/settings.json"), b"{}").unwrap();
        assert_eq!(
            load_settings(root.path()).unwrap_err(),
            "update-settings-invalid"
        );
    }

    #[test]
    fn only_one_updater_can_hold_an_instance_lock() {
        let root = tempfile::tempdir().unwrap();
        let _first = try_lock(root.path()).unwrap();
        assert_eq!(try_lock(root.path()).unwrap_err(), "update-already-running");
    }

    #[test]
    fn ready_helper_has_priority_when_the_parent_releases_the_update_lock() {
        let root = tempfile::tempdir().unwrap();
        let parent = try_lock(root.path()).unwrap();
        let helper = try_helper_lock(root.path()).unwrap();

        drop(parent);
        assert_eq!(try_lock(root.path()).unwrap_err(), "update-already-running");
        drop(try_lock_for_helper(root.path()).unwrap());

        drop(helper);
        drop(try_lock(root.path()).unwrap());
    }

    #[test]
    fn installer_guard_accepts_only_an_exact_lowercase_nonce() {
        let root = tempfile::tempdir().unwrap();
        let nonce = "a".repeat(64);
        assert!(installer_guard_path(root.path(), &nonce, "ready").is_ok());
        for invalid in [format!("{nonce}\r\n"), "A".repeat(64), "g".repeat(64)] {
            assert_eq!(
                installer_guard_path(root.path(), &invalid, "ready").unwrap_err(),
                "installer-guard-invalid"
            );
        }
    }

    #[test]
    fn installer_guard_failure_restores_running_intent_and_records_it() {
        let root = tempfile::tempdir().unwrap();
        let ready = root.path().join("ready");
        let error = root.path().join("error");
        fs::write(&ready, b"ready").unwrap();
        let mut restarted = false;
        let result =
            installer_guard_failure(&ready, &error, true, "installer-guard-write-failed", || {
                restarted = true;
                Ok(())
            });
        assert_eq!(result, "installer-guard-write-failed");
        assert!(restarted);
        assert!(!ready.exists());
        let recorded: serde_json::Value =
            serde_json::from_slice(&fs::read(&error).unwrap()).unwrap();
        assert_eq!(recorded["wasRunning"], true);
        assert_eq!(recorded["restartFailed"], false);

        fs::write(&ready, b"ready").unwrap();
        let result =
            installer_guard_failure(&ready, &error, true, "installer-guard-timeout", || {
                Err("server-start-failed".into())
            });
        assert_eq!(result, "installer-guard-restart-failed");
        let recorded: serde_json::Value =
            serde_json::from_slice(&fs::read(error).unwrap()).unwrap();
        assert_eq!(recorded["restartFailed"], true);
    }

    #[tokio::test]
    async fn installer_guard_cancel_before_stop_does_not_publish_ready() {
        let root = tempfile::tempdir().unwrap();
        let nonce = "b".repeat(64);
        let cancel = installer_guard_path(root.path(), &nonce, "cancel").unwrap();
        write_json(
            &cancel,
            &serde_json::json!({"cancel":true}),
            "test-write-failed",
        )
        .unwrap();
        assert_eq!(
            run_installer_guard(root.path(), &root.path().join("server"), &nonce)
                .await
                .unwrap_err(),
            "installer-guard-cancelled"
        );
        assert!(!cancel.exists());
        assert!(!installer_guard_path(root.path(), &nonce, "ready")
            .unwrap()
            .exists());
    }

    #[test]
    fn installer_parent_wait_outlives_the_server_stop_budget() {
        assert!(INSTALLER_GUARD_PREPARE_TIMEOUT > Duration::from_secs(30));
        assert!(INSTALLER_GUARD_PREPARE_TIMEOUT >= Duration::from_secs(40));
    }

    #[test]
    fn installer_configuration_allows_only_its_applied_directory_transaction() {
        #[cfg(target_os = "macos")]
        if std::env::var_os("RISUNEST_MAC_INSTALLER_CONFIG_TEST_CHILD").is_none() {
            let app = tempfile::tempdir().unwrap();
            let executable = app.path().join("RisuNest Sync Tests.app/Contents/MacOS");
            fs::create_dir_all(&executable).unwrap();
            let executable = executable.join("risunest-sync-manager-tests");
            fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
            let status = std::process::Command::new(executable)
                .arg("update::tests::installer_configuration_allows_only_its_applied_directory_transaction")
                .args(["--exact", "--nocapture"])
                .env("RISUNEST_MAC_INSTALLER_CONFIG_TEST_CHILD", "1")
                .status()
                .unwrap();
            assert!(
                status.success(),
                "managed .app installer configuration child test failed"
            );
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        #[cfg(target_os = "macos")]
        let install = std::env::current_exe()
            .unwrap()
            .ancestors()
            .find(|path| path.extension().is_some_and(|value| value == "app"))
            .unwrap()
            .to_owned();
        #[cfg(not(target_os = "macos"))]
        let install = temp.path().join("install");
        let staged = install
            .parent()
            .unwrap()
            .join(".risunest-sync-update-stage");
        let backup = install
            .parent()
            .unwrap()
            .join(".risunest-sync-update-backup");
        fs::create_dir_all(&install).unwrap();
        fs::create_dir_all(&staged).unwrap();
        #[cfg(target_os = "macos")]
        let server = install.join("Contents/MacOS/risunest-sync-server");
        #[cfg(not(target_os = "macos"))]
        let server = install.join(if cfg!(windows) {
            "risunest-sync-server.exe"
        } else {
            "risunest-sync-server"
        });
        let mut transaction = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Directory,
            install,
            staged,
            backup,
            Vec::new(),
            true,
        )
        .unwrap();
        transaction.save(&root).unwrap();
        assert_eq!(
            validate_installer_configuration(&root, &server).unwrap_err(),
            "update-recovery-required"
        );
        #[cfg(unix)]
        transaction.apply(&root).unwrap();
        #[cfg(windows)]
        {
            transaction.phase = TransactionPhase::Installed;
            transaction.save(&root).unwrap();
        }
        validate_installer_configuration(&root, &server).unwrap();
    }

    #[test]
    fn activity_leases_block_only_while_live() {
        let root = tempfile::tempdir().unwrap();
        set_activity(root.path(), 123, true).unwrap();
        assert!(has_active_activity(root.path()).unwrap());
        set_activity(root.path(), 123, false).unwrap();
        assert!(!has_active_activity(root.path()).unwrap());
        fs::create_dir_all(root.path().join("manager-update")).unwrap();
        fs::write(
            root.path().join("manager-update/activity-456.lease"),
            br#"{"expiresAt":0}"#,
        )
        .unwrap();
        assert!(!has_active_activity(root.path()).unwrap());
        assert!(!root
            .path()
            .join("manager-update/activity-456.lease")
            .exists());
    }

    #[test]
    fn activity_guard_uses_a_process_lifetime_file_lock() {
        let root = tempfile::tempdir().unwrap();
        let guard = ActivityGuard::start(root.path()).unwrap();
        assert!(has_active_activity(root.path()).unwrap());
        drop(guard);
        assert!(!has_active_activity(root.path()).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn failed_atomic_replace_preserves_the_previous_settings_file() {
        use std::os::windows::fs::OpenOptionsExt;

        let root = tempfile::tempdir().unwrap();
        save_policy(root.path(), UpdatePolicy::Off).unwrap();
        let path = root.path().join("manager-update/settings.json");
        let locked = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        assert_eq!(
            save_policy(root.path(), UpdatePolicy::Automatic).unwrap_err(),
            "update-settings-write-failed"
        );
        drop(locked);
        assert_eq!(
            load_settings(root.path()).unwrap().policy,
            UpdatePolicy::Off
        );
    }
}
