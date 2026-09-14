use super::{
    extract_package, finish_rollback_recovery, has_active_activity, helper_active, load_settings,
    load_status, now, recover_transaction, save_status, try_helper_lock, try_lock,
    try_lock_for_helper, validate_installed_marker, InstallTransaction, RecoveryOutcome,
    TransactionKind, TransactionPhase, UpdateLock, UpdatePhase, UpdatePolicy, UpdateStatus,
};
use crate::{client::Client, lifecycle, platform, Result};
use async_trait::async_trait;
use risunest_release_update::{
    Architecture, MetadataLoader, MetadataTransport, OperatingSystem, PackageFormat,
    PackageRequest, Product, Repository, TransportError, Variant,
};
use risunest_sync_server::{PROTOCOL_ID, STORE_FORMAT_ID};
use serde::Serialize;
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const CHECK_INTERVAL: u64 = 6 * 60 * 60;
const DEFER_SECONDS: u64 = 60 * 60;
const IDLE_BUDGET: Duration = Duration::from_secs(10 * 60);
const DRAIN_BUDGET: Duration = Duration::from_secs(60);
const LEASE_RENEWAL: Duration = Duration::from_secs(15);
const HELPER_READY_BUDGET: Duration = Duration::from_secs(5);
const HELPER_LOCK_BUDGET: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunMode {
    Scheduled,
    Manual,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "result", content = "value", rename_all = "kebab-case")]
pub enum RunOutcome {
    Skipped,
    Current,
    Available(String),
    Deferred(String),
    Started(String),
    Completed(String),
}

enum DaemonPresence {
    Running(Value),
    Stopped,
    StartingOrUnreachable,
}

enum HealthObservation {
    Pending,
    Healthy(Option<&'static str>),
    LocalFailure,
}

#[derive(Clone)]
struct HttpTransport {
    client: reqwest::Client,
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if attempt.previous().len() >= 5 {
                return attempt.error("too many redirects");
            }
            let url = attempt.url();
            let host = url.host_str().unwrap_or_default();
            if url.scheme() == "https"
                && url.port().is_none()
                && (host == "github.com" || host.ends_with(".githubusercontent.com"))
            {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }))
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(60))
        .build()
        .map_err(|_| "update-network-unavailable".into())
}

#[async_trait]
impl MetadataTransport for HttpTransport {
    async fn get(
        &self,
        url: &reqwest::Url,
        max_bytes: usize,
        timeout: Duration,
    ) -> std::result::Result<Vec<u8>, TransportError> {
        let mut response = self
            .client
            .get(url.clone())
            .timeout(timeout)
            .send()
            .await
            .map_err(|_| TransportError::new("request failed"))?;
        if !response.status().is_success() {
            return Err(TransportError::new("unexpected HTTP status"));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| TransportError::new("response incomplete"))?
        {
            if bytes.len().saturating_add(chunk.len()) > max_bytes {
                return Err(TransportError::new("response exceeds limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}

fn release_error(error: risunest_release_update::Error) -> String {
    match error {
        risunest_release_update::Error::NotConfigured => "update-not-configured",
        risunest_release_update::Error::InvalidSignature(_) => "update-signature-invalid",
        risunest_release_update::Error::PackageUnavailable => "update-package-unavailable",
        risunest_release_update::Error::DownloadVerification(_) => "update-package-invalid",
        risunest_release_update::Error::UnsafeArchiveEntry(_) => "update-archive-entry-unsafe",
        risunest_release_update::Error::Transport(_) => "update-network-unavailable",
        _ => "update-metadata-invalid",
    }
    .into()
}

fn request() -> Result<PackageRequest> {
    let os = if cfg!(windows) {
        OperatingSystem::Windows
    } else if cfg!(target_os = "macos") {
        OperatingSystem::Darwin
    } else if cfg!(target_os = "linux") {
        OperatingSystem::Linux
    } else {
        return Err("update-platform-unsupported".into());
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => Architecture::X86_64,
        "aarch64" => Architecture::Aarch64,
        _ => return Err("update-architecture-unsupported".into()),
    };
    let format = if cfg!(windows) {
        PackageFormat::Zip
    } else if cfg!(target_os = "macos") {
        PackageFormat::AppTarGz
    } else {
        PackageFormat::TarGz
    };
    Ok(PackageRequest {
        product: Product::Sync,
        variant: Variant::Managed,
        os,
        arch,
        format,
    })
}

fn due(status: &UpdateStatus, current: u64) -> bool {
    if let Some(deferred) = status.deferred_until {
        return deferred <= current;
    }
    status
        .last_checked_at
        .is_none_or(|checked| current.saturating_sub(checked) >= CHECK_INTERVAL)
}

fn initial_delay(root: &Path) -> Duration {
    let jitter = platform::instance_name(root)
        .bytes()
        .fold(0u64, |value, byte| {
            value.wrapping_mul(31).wrapping_add(byte as u64)
        })
        % 300;
    Duration::from_secs(60 + jitter)
}

fn failed_version_retry_due(status: &UpdateStatus, target: &str, current: u64) -> bool {
    status.last_failed_version.as_deref() != Some(target)
        || status
            .last_checked_at
            .is_none_or(|checked| current.saturating_sub(checked) >= CHECK_INTERVAL)
}

fn transition(
    root: &Path,
    status: &mut UpdateStatus,
    phase: UpdatePhase,
    reason: Option<&str>,
) -> Result<()> {
    status.phase = phase;
    status.reason = reason.map(str::to_owned);
    save_status(root, status)
}

fn defer(root: &Path, status: &mut UpdateStatus, reason: &str) -> Result<RunOutcome> {
    status.deferred_until = Some(now() + DEFER_SECONDS);
    transition(root, status, UpdatePhase::Deferred, Some(reason))?;
    Ok(RunOutcome::Deferred(reason.into()))
}

async fn fetch_file(client: &reqwest::Client, url: &str, expected: u64, path: &Path) -> Result<()> {
    let url = reqwest::Url::parse(url).map_err(|_| "update-download-url-invalid".to_owned())?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| "update-download-failed".to_owned())?;
    if !response.status().is_success() {
        return Err("update-download-failed".into());
    }
    if response
        .content_length()
        .is_some_and(|length| length != expected)
    {
        return Err("update-package-size-mismatch".into());
    }
    let mut file = File::create(path).map_err(|_| "update-staging-create-failed".to_owned())?;
    let mut received = 0u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "update-download-failed".to_owned())?
    {
        received = received.saturating_add(chunk.len() as u64);
        if received > expected {
            return Err("update-package-size-mismatch".into());
        }
        file.write_all(&chunk)
            .map_err(|_| "update-staging-write-failed".to_owned())?;
    }
    file.sync_all()
        .map_err(|_| "update-staging-write-failed".to_owned())?;
    if received != expected {
        return Err("update-package-size-mismatch".into());
    }
    Ok(())
}

pub(super) fn install_path(server: &Path) -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        let current = std::env::current_exe().map_err(|_| "executable-unavailable".to_owned())?;
        return current
            .ancestors()
            .find(|path| path.extension().is_some_and(|value| value == "app"))
            .map(Path::to_owned)
            .ok_or("managed-app-bundle-required".into());
    }
    #[cfg(not(target_os = "macos"))]
    server
        .parent()
        .map(Path::to_owned)
        .ok_or("managed-install-path-invalid".into())
}

fn stage_paths(install: &Path, root: &Path) -> Result<(PathBuf, PathBuf, PathBuf)> {
    let parent = install.parent().ok_or("managed-install-path-invalid")?;
    let suffix = platform::instance_name(root);
    Ok((
        parent.join(format!(".risunest-sync-update-unpack-{suffix}")),
        parent.join(format!(".risunest-sync-update-stage-{suffix}")),
        parent.join(format!(".risunest-sync-update-backup-{suffix}")),
    ))
}

fn prepare_stage(path: &Path, required: u64) -> Result<()> {
    let parent = path.parent().ok_or("managed-install-path-invalid")?;
    let free =
        fs2::available_space(parent).map_err(|_| "update-disk-space-unavailable".to_owned())?;
    if free < required.saturating_mul(3).saturating_add(16 * 1024 * 1024) {
        return Err("update-disk-space-insufficient".into());
    }
    let probe = parent.join(format!(
        ".risunest-sync-update-probe-{}",
        std::process::id()
    ));
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .map_err(|_| "update-install-not-writable".to_owned())?;
    fs::remove_file(probe).map_err(|_| "update-install-not-writable".to_owned())
}

#[cfg(target_os = "macos")]
fn validate_native_bundle(path: &Path) -> Result<()> {
    let status = platform::process("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(path)
        .status()
        .map_err(|_| "update-native-signature-unavailable".to_owned())?;
    if status.success() {
        Ok(())
    } else {
        Err("update-native-signature-invalid".into())
    }
}

#[cfg(not(target_os = "macos"))]
fn validate_native_bundle(_path: &Path) -> Result<()> {
    Ok(())
}

async fn wait_for_idle(client: &Client, root: &Path, status: &mut UpdateStatus) -> Result<bool> {
    transition(root, status, UpdatePhase::WaitingIdle, None)?;
    let deadline = Instant::now() + IDLE_BUDGET;
    loop {
        if has_active_activity(root)? {
            return Ok(false);
        }
        let maintenance = client.maintenance().await?;
        if maintenance.idle_eligible {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn drain_and_shutdown(
    client: &Client,
    root: &Path,
    locator: &risunest_sync_server::management::discovery::Discovery,
    status: &mut UpdateStatus,
) -> Result<bool> {
    async fn release_lease(
        client: &Client,
        locator: &risunest_sync_server::management::discovery::Discovery,
        lease: &crate::client::MaintenanceLease,
    ) {
        let revision = match client.maintenance_with_locator(locator).await {
            Ok(current) if current.state == "draining" => current.revision,
            _ => lease.status.revision.clone(),
        };
        let _ = client
            .maintenance_lease_action_with_locator(
                locator,
                "release",
                &revision,
                &lease.lease_token,
            )
            .await;
    }

    let initial = client.maintenance_with_locator(locator).await?;
    let mut lease = client
        .acquire_maintenance_with_locator(locator, &initial.revision)
        .await?;
    if let Err(error) = transition(root, status, UpdatePhase::Draining, None) {
        release_lease(client, locator, &lease).await;
        return Err(error);
    }
    let deadline = Instant::now() + DRAIN_BUDGET;
    let mut renewed = Instant::now();
    loop {
        let active = match has_active_activity(root) {
            Ok(active) => active,
            Err(error) => {
                release_lease(client, locator, &lease).await;
                return Err(error);
            }
        };
        if active {
            release_lease(client, locator, &lease).await;
            return Ok(false);
        }
        let current = match client.maintenance_with_locator(locator).await {
            Ok(current) => current,
            Err(error) => {
                release_lease(client, locator, &lease).await;
                return Err(error);
            }
        };
        lease.status = current;
        if lease.status.drained {
            let shutdown = client
                .shutdown_maintenance_with_locator(
                    locator,
                    &lease.status.revision,
                    &lease.lease_token,
                )
                .await;
            return match shutdown {
                Ok(_) => Ok(true),
                Err(_) => {
                    match lifecycle::wait_stopped(root, locator, Duration::from_secs(30)).await {
                        Ok(()) => Ok(true),
                        Err(_) => match client.maintenance_with_locator(locator).await {
                            Ok(current) if current.state == "draining" => {
                                let _ = client
                                    .maintenance_lease_action_with_locator(
                                        locator,
                                        "release",
                                        &current.revision,
                                        &lease.lease_token,
                                    )
                                    .await;
                                Ok(false)
                            }
                            Ok(current) if current.state == "open" => Ok(false),
                            _ => Err("update-shutdown-ambiguous".into()),
                        },
                    }
                }
            };
        }
        if Instant::now() >= deadline {
            release_lease(client, locator, &lease).await;
            return Ok(false);
        }
        if renewed.elapsed() >= LEASE_RENEWAL {
            lease.status = match client
                .maintenance_lease_action_with_locator(
                    locator,
                    "renew",
                    &lease.status.revision,
                    &lease.lease_token,
                )
                .await
            {
                Ok(status) => status,
                Err(error) => {
                    release_lease(client, locator, &lease).await;
                    return Err(error);
                }
            };
            renewed = Instant::now();
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn daemon_presence(client: &Client, root: &Path, server: &Path) -> Result<DaemonPresence> {
    let locator = lifecycle::locator_exists(root)?;
    match client.status().await {
        Ok(status) => return Ok(DaemonPresence::Running(status)),
        Err(error) if error == "daemon-unavailable" || (!locator && error == "storage-io") => {}
        Err(error) => return Err(error),
    }
    if lifecycle::owner_active(root)? {
        return Ok(DaemonPresence::StartingOrUnreachable);
    }
    let registered = match platform::startup(root, server, "status") {
        Ok(status) => status.registered && status.enabled,
        Err(_) => return Ok(DaemonPresence::StartingOrUnreachable),
    };
    let attempts = if registered { 59 } else { 1 };
    for _ in 0..attempts {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let locator = lifecycle::locator_exists(root)?;
        match client.status().await {
            Ok(status) => return Ok(DaemonPresence::Running(status)),
            Err(error) if error == "daemon-unavailable" || (!locator && error == "storage-io") => {
                if lifecycle::owner_active(root)? {
                    return Ok(DaemonPresence::StartingOrUnreachable);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(DaemonPresence::Stopped)
}

fn classify_health(status: &Value) -> HealthObservation {
    let publication = || match status["publication"]["phase"].as_str() {
        Some("published" | "disabled" | "stopped") => HealthObservation::Healthy(None),
        Some("failed") => match status["publication"]["error"].as_str() {
            Some("directory-unreachable" | "directory-publication-failed") => {
                HealthObservation::Healthy(Some("external-connectivity-pending"))
            }
            _ => HealthObservation::LocalFailure,
        },
        _ => HealthObservation::Pending,
    };
    if status["connectionState"]["mode"] != "managed" {
        return publication();
    }
    match status["tunnel"]["phase"].as_str() {
        Some("connected") => publication(),
        Some("starting") => HealthObservation::Pending,
        Some("failed") => match status["tunnel"]["error"].as_str() {
            Some("tunnel-readiness-timeout") => {
                HealthObservation::Healthy(Some("external-connectivity-pending"))
            }
            _ => HealthObservation::LocalFailure,
        },
        _ => HealthObservation::LocalFailure,
    }
}

pub(super) fn copy_helper(root: &Path) -> Result<PathBuf> {
    let current = platform::manager_executable()?;
    if !current.is_file() {
        return Err("manager-executable-missing".into());
    }
    let directory = root.join("manager-update/helper");
    fs::create_dir_all(&directory).map_err(|_| "update-helper-write-failed".to_owned())?;
    let name = if cfg!(windows) {
        "risunest-sync-update-helper.exe"
    } else {
        "risunest-sync-update-helper"
    };
    let target = directory.join(name);
    let temporary = directory.join(format!("{name}.new"));
    fs::copy(current, &temporary).map_err(|_| "update-helper-write-failed".to_owned())?;
    if target.exists() {
        fs::remove_file(&target).map_err(|_| "update-helper-write-failed".to_owned())?;
    }
    fs::rename(temporary, &target).map_err(|_| "update-helper-write-failed".to_owned())?;
    Ok(target)
}

async fn cancel_prepared_after_shutdown(
    root: &Path,
    server: &Path,
    transaction: &InstallTransaction,
) -> Result<()> {
    if transaction.was_running {
        lifecycle::wait_owner_released(root, Duration::from_secs(120)).await?;
        platform::start(root, server)?;
        wait_healthy(root, &transaction.source_version).await?;
    }
    transaction.cancel_prepared(root)?;
    Ok(())
}

async fn cancel_prepared_with_running_source(
    root: &Path,
    server: &Path,
    transaction: &InstallTransaction,
) -> Result<()> {
    if transaction.was_running {
        let client = Client::new(root.to_owned())?;
        let status = tokio::time::timeout(Duration::from_secs(3), client.status()).await;
        let source_is_stably_running = matches!(status, Ok(Ok(value))
            if value["version"] == transaction.source_version
                && value["protocolId"] == PROTOCOL_ID
                && value["storeFormatId"] == STORE_FORMAT_ID
                && matches!(value["maintenance"]["state"].as_str(), Some("open" | "draining")));
        if !source_is_stably_running {
            return cancel_prepared_after_shutdown(root, server, transaction).await;
        }
    }
    transaction.cancel_prepared(root)
}

pub async fn run(root: &Path, server: &Path, mode: RunMode) -> Result<RunOutcome> {
    let delay = if mode == RunMode::Scheduled {
        let initial_lock = try_lock(root)?;
        let delay = scheduled_initial_delay(root, server, &initial_lock)?;
        drop(initial_lock);
        delay
    } else {
        None
    };
    if let Some(delay) = delay {
        tokio::time::sleep(delay).await;
    }
    let lock = try_lock(root)?;
    run_while_locked(root, server, mode, &lock).await
}

fn scheduled_initial_delay(
    root: &Path,
    server: &Path,
    _lock: &UpdateLock,
) -> Result<Option<Duration>> {
    let install = install_path(server)?;
    if InstallTransaction::load(root, &install)?.is_some() {
        return Ok(None);
    }
    let settings = load_settings(root)?;
    let status = load_status(root)?;
    let current_time = now();
    if settings.policy == UpdatePolicy::Off || !due(&status, current_time) {
        return Ok(None);
    }
    Ok(
        (status.last_checked_at.is_none() && status.phase == UpdatePhase::Idle)
            .then(|| initial_delay(root)),
    )
}

pub async fn run_while_locked(
    root: &Path,
    server: &Path,
    mode: RunMode,
    _lock: &super::UpdateLock,
) -> Result<RunOutcome> {
    match run_inner_locked(root, server, mode).await {
        Ok(outcome) => Ok(outcome),
        Err(error) if error == "update-already-running" => Err(error),
        Err(error) => {
            if let Ok(mut status) = load_status(root) {
                status.last_failed_version = status.target_version.clone();
                let _ = transition(root, &mut status, UpdatePhase::Failed, Some(&error));
            }
            Err(error)
        }
    }
}

async fn recover_locked(root: &Path, server: &Path, install: &Path) -> Result<Option<RunOutcome>> {
    if let Some(transaction) = InstallTransaction::load(root, install)? {
        if matches!(
            transaction.phase,
            TransactionPhase::BackupMoved
                | TransactionPhase::Installing
                | TransactionPhase::Installed
                | TransactionPhase::Restarting
                | TransactionPhase::RollingBack
        ) {
            let client = Client::new(root.to_owned())?;
            lifecycle::stop(root, &client).await?;
        }
    }
    let Some(recovery) = recover_transaction(root, install)? else {
        return Ok(None);
    };
    let mut recovered = load_status(root)?;
    match recovery {
        RecoveryOutcome::Completed { target_version } => {
            recovered.last_completed_at = Some(now());
            recovered.last_failed_version = None;
            transition(root, &mut recovered, UpdatePhase::Completed, None)?;
            Ok(Some(RunOutcome::Completed(target_version)))
        }
        RecoveryOutcome::RolledBack {
            was_running,
            source_version,
            target_version,
            installer_startup_enabled,
        } => {
            let restored = if let Some(enabled) = installer_startup_enabled {
                super::restore_installer_state(root, server, was_running, enabled, &source_version)
                    .await
            } else if was_running {
                match platform::start(root, server) {
                    Ok(()) => wait_healthy(root, &source_version).await.map(|_| ()),
                    Err(error) => Err(error),
                }
            } else {
                Ok(())
            };
            if restored.is_err() {
                recovered.last_failed_version = Some(target_version);
                transition(
                    root,
                    &mut recovered,
                    UpdatePhase::Failed,
                    Some("update-rollback-restart-failed"),
                )?;
                return Err("update-rollback-restart-failed".into());
            }
            finish_rollback_recovery(root, install)?;
            recovered.last_failed_version = Some(target_version);
            transition(
                root,
                &mut recovered,
                UpdatePhase::Failed,
                Some("update-interrupted-and-rolled-back"),
            )?;
            Err("update-interrupted-and-rolled-back".into())
        }
    }
}

async fn wait_for_helper_ready(root: &Path) -> Result<()> {
    let deadline = Instant::now() + HELPER_READY_BUDGET;
    loop {
        if helper_active(root)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("update-helper-start-failed".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn acquire_helper_update_lock(root: &Path) -> Result<UpdateLock> {
    let deadline = Instant::now() + HELPER_LOCK_BUDGET;
    loop {
        match try_lock_for_helper(root) {
            Ok(lock) => return Ok(lock),
            Err(error) if error == "update-already-running" => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err("update-helper-lock-timeout".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn run_inner_locked(root: &Path, server: &Path, mode: RunMode) -> Result<RunOutcome> {
    let install = install_path(server)?;
    #[cfg(windows)]
    if let Some(transaction) = InstallTransaction::load(root, &install)? {
        if transaction.kind == TransactionKind::Files
            && matches!(
                transaction.phase,
                TransactionPhase::BackupMoved
                    | TransactionPhase::Installing
                    | TransactionPhase::Installed
                    | TransactionPhase::Restarting
                    | TransactionPhase::RollingBack
            )
        {
            let helper = copy_helper(root)?;
            let mut command = Command::new(helper);
            command
                .args(["--data-dir"])
                .arg(root)
                .args(["--server"])
                .arg(server)
                .args(["update", "recover-helper", &std::process::id().to_string()])
                .arg(&install);
            let helper_started = match platform::spawn_update_helper(root, &mut command) {
                Ok(_) => wait_for_helper_ready(root).await,
                Err(error) => Err(error),
            };
            if let Err(error) = helper_started {
                platform::cleanup_update_helpers(root)?;
                return Err(error);
            }
            return Ok(RunOutcome::Started(transaction.target_version));
        }
    }
    if let Some(outcome) = recover_locked(root, server, &install).await? {
        return Ok(outcome);
    }
    let settings = load_settings(root)?;
    let mut status = load_status(root)?;
    let current_time = now();
    if mode == RunMode::Scheduled
        && (settings.policy == UpdatePolicy::Off || !due(&status, current_time))
    {
        return Ok(RunOutcome::Skipped);
    }
    let prior_status = status.clone();
    let installed_files = validate_installed_marker(&install, env!("CARGO_PKG_VERSION"))?;
    status.deferred_until = None;
    transition(root, &mut status, UpdatePhase::Checking, None)?;

    let key_text = option_env!("RISUNEST_UPDATE_PUBLIC_KEY").unwrap_or("");
    let http = http_client()?;
    let loader = MetadataLoader::new(
        Repository::risunest(),
        key_text,
        HttpTransport {
            client: http.clone(),
        },
    )
    .map_err(release_error)?;
    let product = match loader.load_latest(Product::Sync).await {
        Ok(product) => product,
        Err(error) => {
            let error = release_error(error);
            if error == "update-network-unavailable" {
                return defer(root, &mut status, "update-network-unavailable");
            }
            status.last_checked_at = Some(current_time);
            save_status(root, &status)?;
            return Err(error);
        }
    };
    status.last_checked_at = Some(current_time);
    save_status(root, &status)?;
    let target = product.release.version.clone();
    status.target_version = Some(target.clone());
    let current_version = env!("CARGO_PKG_VERSION");
    let target_version = product.release.semver().map_err(release_error)?;
    let current_semver = semver::Version::parse(current_version)
        .map_err(|_| "installed-version-invalid".to_owned())?;
    if target_version <= current_semver {
        status.target_version = None;
        status.deferred_until = None;
        transition(
            root,
            &mut status,
            UpdatePhase::Completed,
            Some("already-current"),
        )?;
        return Ok(RunOutcome::Current);
    }
    if mode == RunMode::Scheduled && !failed_version_retry_due(&prior_status, &target, current_time)
    {
        transition(
            root,
            &mut status,
            UpdatePhase::Failed,
            Some("previous-attempt-failed"),
        )?;
        return Ok(RunOutcome::Available(target));
    }
    let compatibility = product
        .release
        .compatibility
        .as_ref()
        .ok_or("update-compatibility-missing")?;
    let compatible = compatibility.protocol_id == PROTOCOL_ID
        && compatibility.store_format_id == STORE_FORMAT_ID
        && compatibility.automatic_apply;
    if settings.policy == UpdatePolicy::Notify || !compatible {
        transition(
            root,
            &mut status,
            UpdatePhase::Completed,
            Some(if compatible {
                "update-available"
            } else {
                "manual-update-required"
            }),
        )?;
        return Ok(RunOutcome::Available(target));
    }
    if has_active_activity(root)? {
        return defer(root, &mut status, "management-active");
    }
    let request = request()?;
    let download = product
        .select_download(&request)
        .map_err(release_error)?
        .clone();
    let (unpack, staged, backup) = stage_paths(&install, root)?;
    prepare_stage(&unpack, download.size)?;
    for path in [&unpack, &staged] {
        if path.exists() {
            fs::remove_dir_all(path).map_err(|_| "update-staging-cleanup-failed".to_owned())?;
        }
    }
    if backup.exists() {
        return Err("update-recovery-required".into());
    }
    let package_dir = root.join("manager-update/download");
    fs::create_dir_all(&package_dir).map_err(|_| "update-staging-create-failed".to_owned())?;
    let package = package_dir.join("package.download");
    transition(root, &mut status, UpdatePhase::Downloading, None)?;
    fetch_file(&http, &download.url, download.size, &package).await?;
    let signature_bytes = HttpTransport {
        client: http.clone(),
    }
    .get(
        &reqwest::Url::parse(&download.signature_url).map_err(|_| "update-download-url-invalid")?,
        128 * 1024,
        Duration::from_secs(10),
    )
    .await
    .map_err(|_| "update-signature-download-failed".to_owned())?;
    let signature =
        std::str::from_utf8(&signature_bytes).map_err(|_| "update-signature-invalid".to_owned())?;
    product
        .verify_signed_download_file(&download, &package, signature)
        .map_err(release_error)?;
    let extracted = extract_package(
        download.format,
        &package,
        &unpack,
        &target,
        &product.release.vendor,
        request.os,
        request.arch,
    )?;
    let (transaction_stage, kind, files, removed_files) = if cfg!(target_os = "macos") {
        fs::rename(&extracted.root, &staged)
            .map_err(|_| "update-staging-move-failed".to_owned())?;
        #[cfg(unix)]
        File::open(staged.parent().ok_or("update-staging-move-failed")?)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| "update-staging-move-failed".to_owned())?;
        fs::remove_dir(&unpack).map_err(|_| "update-staging-move-failed".to_owned())?;
        #[cfg(unix)]
        File::open(unpack.parent().ok_or("update-staging-move-failed")?)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| "update-staging-move-failed".to_owned())?;
        (
            staged.clone(),
            TransactionKind::Directory,
            Vec::new(),
            Vec::new(),
        )
    } else if cfg!(windows) {
        let desired = extracted.files.iter().cloned().collect::<BTreeSet<_>>();
        let removed = installed_files
            .into_iter()
            .filter(|path| !desired.contains(path))
            .collect();
        (
            extracted.root,
            TransactionKind::Files,
            extracted.files,
            removed,
        )
    } else {
        (
            extracted.root,
            TransactionKind::Directory,
            Vec::new(),
            Vec::new(),
        )
    };
    validate_native_bundle(&transaction_stage)?;

    let client = Client::new(root.to_owned())?;
    let daemon = match daemon_presence(&client, root, server).await? {
        DaemonPresence::Running(value) => Some(value),
        DaemonPresence::Stopped => None,
        DaemonPresence::StartingOrUnreachable => {
            return defer(root, &mut status, "server-starting-or-unreachable")
        }
    };
    if let Some(value) = &daemon {
        if value["version"] != current_version
            || value["protocolId"] != PROTOCOL_ID
            || value["storeFormatId"] != STORE_FORMAT_ID
        {
            return Err("installed-version-or-compatibility-mismatch".into());
        }
        if !wait_for_idle(&client, root, &mut status).await? {
            return defer(root, &mut status, "server-busy");
        }
    }
    let transaction = InstallTransaction::new_with_removed(
        current_version.into(),
        target.clone(),
        kind,
        install.clone(),
        transaction_stage,
        backup,
        files,
        removed_files,
        daemon.is_some(),
    )?;
    let manager = platform::manager_executable()?;
    let mut live_processes = vec![
        install.join(server.file_name().ok_or("server-path-invalid")?),
        install.join(manager.file_name().ok_or("manager-path-invalid")?),
        install.join(if cfg!(windows) {
            "cloudflared.exe"
        } else {
            "cloudflared"
        }),
    ];
    if mode == RunMode::Manual {
        let invoker = std::env::current_exe().map_err(|_| "executable-unavailable".to_owned())?;
        if invoker.parent() == Some(install.as_path()) {
            let relative = invoker
                .strip_prefix(&install)
                .map_err(|_| "executable-unavailable".to_owned())?;
            if transaction
                .file_states
                .iter()
                .any(|file| file.path == relative && file.desired_present)
            {
                live_processes.push(invoker);
            }
        }
    }
    if transaction
        .preflight_writable_except(&live_processes)
        .is_err()
    {
        return defer(root, &mut status, "management-app-open-or-install-locked");
    }
    let helper = copy_helper(root)?;
    transaction.save(root)?;
    let mut shutdown_pending = false;
    if daemon.is_some() {
        let locator = match risunest_sync_server::management::discovery::Discovery::load(root) {
            Ok(locator) => locator,
            Err(error) => {
                let _ = transaction.cancel_prepared(root);
                return Err(error.code.to_owned());
            }
        };
        match drain_and_shutdown(&client, root, &locator, &mut status).await {
            Ok(true) => {}
            Ok(false) => {
                transaction.cancel_prepared(root)?;
                return defer(root, &mut status, "server-drain-timeout");
            }
            Err(error) => {
                if error == "update-shutdown-ambiguous" {
                    shutdown_pending = true;
                } else {
                    if cancel_prepared_with_running_source(root, server, &transaction)
                        .await
                        .is_err()
                    {
                        return Err("update-rollback-restart-failed".into());
                    }
                    return Err(error);
                }
            }
        }
        if !shutdown_pending {
            if let Err(error) =
                lifecycle::wait_stopped(root, &locator, Duration::from_secs(30)).await
            {
                if cancel_prepared_with_running_source(root, server, &transaction)
                    .await
                    .is_err()
                {
                    return Err("update-rollback-restart-failed".into());
                }
                return Err(error);
            }
        }
    }
    let _ = transition(root, &mut status, UpdatePhase::Installing, None);
    let mut command = Command::new(helper);
    command
        .args(["--data-dir"])
        .arg(root)
        .args(["--server"])
        .arg(server)
        .args(["update", "helper", &std::process::id().to_string()])
        .arg(&install);
    let helper_started = match platform::spawn_update_helper(root, &mut command) {
        Ok(_) => wait_for_helper_ready(root).await,
        Err(error) => Err(error),
    };
    if let Err(error) = helper_started {
        let helper_cleanup = platform::cleanup_update_helpers(root);
        if cancel_prepared_after_shutdown(root, server, &transaction)
            .await
            .is_err()
        {
            return Err("update-rollback-restart-failed".into());
        }
        helper_cleanup?;
        return Err(error);
    }
    Ok(RunOutcome::Started(target))
}

async fn wait_healthy(root: &Path, expected_version: &str) -> Result<Option<&'static str>> {
    let client = Client::new(root.to_owned())?;
    let deadline = Instant::now() + Duration::from_secs(100);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let attempt = tokio::time::timeout(remaining.min(Duration::from_secs(2)), client.status());
        if let Ok(Ok(status)) = attempt.await {
            if status["version"] == expected_version
                && status["protocolId"] == PROTOCOL_ID
                && status["storeFormatId"] == STORE_FORMAT_ID
            {
                match classify_health(&status) {
                    HealthObservation::Healthy(reason) => return Ok(reason),
                    HealthObservation::LocalFailure => {
                        return Err("updated-server-local-runtime-failed".into())
                    }
                    HealthObservation::Pending => {}
                }
            }
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        tokio::time::sleep(remaining.min(Duration::from_millis(200))).await;
    }
    Err("updated-server-health-failed".into())
}

pub(super) async fn start_and_verify(root: &Path, server: &Path, version: &str) -> Result<()> {
    platform::start(root, server)?;
    wait_healthy(root, version).await.map(|_| ())
}

async fn stop_if_running(root: &Path) -> Result<()> {
    let client = Client::new(root.to_owned())?;
    lifecycle::stop(root, &client).await
}

async fn restore_previous(
    root: &Path,
    server: &Path,
    transaction: &mut InstallTransaction,
) -> Result<()> {
    stop_if_running(root).await?;
    transaction.rollback(root)?;
    if transaction.was_running {
        platform::start(root, server)?;
        wait_healthy(root, &transaction.source_version).await?;
    }
    Ok(())
}

pub async fn run_helper(
    root: &Path,
    server: &Path,
    parent: u32,
    install: &Path,
) -> Result<RunOutcome> {
    let _helper_lock = try_helper_lock(root)?;
    let _lock = acquire_helper_update_lock(root).await?;
    if let Err(parent_error) = platform::wait_for_parent_exit(parent, Duration::from_secs(30)) {
        let transaction =
            InstallTransaction::load(root, install)?.ok_or("update-transaction-missing")?;
        let mut status = load_status(root)?;
        if transaction.phase != TransactionPhase::Prepared {
            return Err(parent_error);
        }
        if transaction.was_running
            && (lifecycle::wait_owner_released(root, Duration::from_secs(120))
                .await
                .is_err()
                || platform::start(root, server).is_err()
                || wait_healthy(root, &transaction.source_version)
                    .await
                    .is_err())
        {
            status.last_failed_version = Some(transaction.target_version.clone());
            transition(
                root,
                &mut status,
                UpdatePhase::Failed,
                Some("update-rollback-restart-failed"),
            )?;
            return Err("update-rollback-restart-failed".into());
        }
        transaction.cancel_prepared(root)?;
        status.last_failed_version = Some(transaction.target_version);
        transition(root, &mut status, UpdatePhase::Failed, Some(&parent_error))?;
        return Err(parent_error);
    }
    let mut transaction =
        InstallTransaction::load(root, install)?.ok_or("update-transaction-missing")?;
    let mut status = load_status(root)?;
    if transaction.was_running {
        if let Err(error) = lifecycle::wait_owner_released(root, Duration::from_secs(120)).await {
            status.last_failed_version = Some(transaction.target_version.clone());
            transition(root, &mut status, UpdatePhase::Failed, Some(&error))?;
            return Err(error);
        }
    }
    let apply = transaction
        .preflight_writable()
        .and_then(|()| transaction.apply(root));
    if let Err(error) = apply {
        let rollback = restore_previous(root, server, &mut transaction).await;
        status.last_failed_version = Some(transaction.target_version.clone());
        transition(
            root,
            &mut status,
            UpdatePhase::Failed,
            Some(if rollback.is_ok() {
                &error
            } else {
                "update-rollback-failed"
            }),
        )?;
        return Err(error);
    }
    transaction.mark_restarting(root)?;
    transition(root, &mut status, UpdatePhase::Restarting, None)?;
    let mut temporary_registration = false;
    let registration = if transaction.was_running {
        Ok(())
    } else {
        match platform::startup(root, server, "status") {
            Ok(startup) if startup.registered && startup.enabled && startup.action_matches => {
                Ok(())
            }
            Ok(startup) if !startup.registered => {
                temporary_registration = true;
                match platform::startup(root, server, "install") {
                    Ok(_) => Ok(()),
                    Err(error) => {
                        if platform::startup(root, server, "remove").is_ok() {
                            temporary_registration = false;
                            Err(error)
                        } else {
                            Err("update-rollback-failed".into())
                        }
                    }
                }
            }
            Ok(_) => Err("update-startup-state-not-verifiable".into()),
            Err(error) => Err(error),
        }
    };
    let mut health = match registration {
        Ok(()) => match platform::start(root, server) {
            Ok(()) => wait_healthy(root, &transaction.target_version).await,
            Err(error) => Err(error),
        },
        Err(error) => Err(error),
    };
    if health.is_ok() && !transaction.was_running {
        let client = Client::new(root.to_owned())?;
        if let Err(error) = lifecycle::stop(root, &client).await {
            health = Err(error);
        }
        if temporary_registration {
            if let Err(error) = platform::startup(root, server, "remove") {
                health = Err(error);
            }
        }
    }
    match health {
        Ok(external_reason) => {
            transaction.complete(root)?;
            status.last_completed_at = Some(now());
            status.last_failed_version = None;
            status.deferred_until = None;
            transition(root, &mut status, UpdatePhase::Completed, external_reason)?;
            Ok(RunOutcome::Completed(transaction.target_version))
        }
        Err(error) => {
            let rollback = restore_previous(root, server, &mut transaction).await;
            let registration_cleanup =
                !temporary_registration || platform::startup(root, server, "remove").is_ok();
            status.last_failed_version = Some(transaction.target_version.clone());
            if rollback.is_err() || !registration_cleanup {
                transition(
                    root,
                    &mut status,
                    UpdatePhase::Failed,
                    Some("update-rollback-failed"),
                )?;
                Err("update-rollback-failed".into())
            } else {
                transition(root, &mut status, UpdatePhase::Failed, Some(&error))?;
                Err(error)
            }
        }
    }
}

pub async fn run_recovery_helper(
    root: &Path,
    server: &Path,
    parent: u32,
    install: &Path,
) -> Result<RunOutcome> {
    let _helper_lock = try_helper_lock(root)?;
    let _lock = acquire_helper_update_lock(root).await?;
    platform::wait_for_parent_exit(parent, Duration::from_secs(30))?;
    recover_locked(root, server, install)
        .await?
        .ok_or("update-transaction-missing".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs2::FileExt;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::Arc,
    };

    #[tokio::test]
    async fn ready_helper_retries_a_transient_instance_lock_holder() {
        let root = tempfile::tempdir().unwrap();
        let holder = try_lock_for_helper(root.path()).unwrap();
        let helper = try_helper_lock(root.path()).unwrap();
        let acquire = acquire_helper_update_lock(root.path());
        tokio::pin!(acquire);

        assert!(
            tokio::time::timeout(Duration::from_millis(75), &mut acquire)
                .await
                .is_err()
        );
        drop(holder);
        drop(
            tokio::time::timeout(Duration::from_secs(1), &mut acquire)
                .await
                .unwrap()
                .unwrap(),
        );
        drop(helper);
    }

    #[test]
    fn scheduled_checks_observe_six_hour_interval_and_one_hour_defer() {
        let status = UpdateStatus {
            last_checked_at: Some(100),
            ..UpdateStatus::default()
        };
        assert!(!due(&status, 100 + CHECK_INTERVAL - 1));
        assert!(due(&status, 100 + CHECK_INTERVAL));
        let deferred = UpdateStatus {
            last_checked_at: Some(100),
            deferred_until: Some(500),
            ..UpdateStatus::default()
        };
        assert!(!due(&deferred, 499));
        assert!(due(&deferred, 500));
    }

    #[test]
    fn first_scheduled_jitter_is_decided_under_a_lock_that_can_be_released_before_sleep() {
        #[cfg(target_os = "macos")]
        if std::env::var_os("RISUNEST_MAC_JITTER_TEST_CHILD").is_none() {
            let app = tempfile::tempdir().unwrap();
            let executable = app.path().join("RisuNest Sync Tests.app/Contents/MacOS");
            std::fs::create_dir_all(&executable).unwrap();
            let executable = executable.join("risunest-sync-manager-tests");
            std::fs::copy(std::env::current_exe().unwrap(), &executable).unwrap();
            let status = std::process::Command::new(executable)
                .arg("update::engine::tests::first_scheduled_jitter_is_decided_under_a_lock_that_can_be_released_before_sleep")
                .args(["--exact", "--nocapture"])
                .env("RISUNEST_MAC_JITTER_TEST_CHILD", "1")
                .status()
                .unwrap();
            assert!(status.success(), "managed .app jitter child test failed");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        #[cfg(target_os = "macos")]
        let server = std::env::current_exe()
            .unwrap()
            .ancestors()
            .find(|path| path.extension().is_some_and(|value| value == "app"))
            .unwrap()
            .join("Contents/MacOS/risunest-sync-server");
        #[cfg(not(target_os = "macos"))]
        let server = root.path().join("managed-install/server");
        let lock = try_lock(root.path()).unwrap();
        let delay = scheduled_initial_delay(root.path(), &server, &lock)
            .unwrap()
            .unwrap();
        assert!(delay >= Duration::from_secs(60));
        assert!(delay <= Duration::from_secs(359));
        drop(lock);
        drop(try_lock(root.path()).unwrap());

        InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Directory,
            install_path(&server).unwrap(),
            root.path().join(".risunest-sync-update-stage"),
            root.path().join(".risunest-sync-update-backup"),
            Vec::new(),
            false,
        )
        .unwrap()
        .save(root.path())
        .unwrap();
        let lock = try_lock(root.path()).unwrap();
        assert_eq!(
            scheduled_initial_delay(root.path(), &server, &lock).unwrap(),
            None
        );
    }

    #[test]
    fn package_selection_is_managed_and_native() {
        let request = request().unwrap();
        assert_eq!(request.product, Product::Sync);
        assert_eq!(request.variant, Variant::Managed);
        if cfg!(windows) {
            assert_eq!(request.format, PackageFormat::Zip);
        }
    }

    #[test]
    fn failed_target_is_not_retried_before_the_next_regular_check() {
        let status = UpdateStatus {
            last_checked_at: Some(100),
            last_failed_version: Some("2.0.0".into()),
            ..UpdateStatus::default()
        };
        assert!(!failed_version_retry_due(
            &status,
            "2.0.0",
            100 + CHECK_INTERVAL - 1
        ));
        assert!(failed_version_retry_due(
            &status,
            "2.0.0",
            100 + CHECK_INTERVAL
        ));
        assert!(failed_version_retry_due(&status, "2.0.1", 101));
    }

    #[test]
    fn health_waits_for_publication_and_splits_local_from_external_failure() {
        let base = serde_json::json!({
            "connectionState":{"mode":"managed"},
            "tunnel":{"phase":"connected","error":null},
            "publication":{"phase":"waiting"}
        });
        assert!(matches!(classify_health(&base), HealthObservation::Pending));
        let local = serde_json::json!({
            "connectionState":{"mode":"managed"},
            "tunnel":{"phase":"failed","error":"tunnel-start-failed"},
            "publication":{"phase":"waiting"}
        });
        assert!(matches!(
            classify_health(&local),
            HealthObservation::LocalFailure
        ));
        let external = serde_json::json!({
            "connectionState":{"mode":"managed"},
            "tunnel":{"phase":"failed","error":"tunnel-readiness-timeout"},
            "publication":{"phase":"failed"}
        });
        assert!(matches!(
            classify_health(&external),
            HealthObservation::Healthy(Some("external-connectivity-pending"))
        ));
        let publication_outage = serde_json::json!({
            "connectionState":{"mode":"managed"},
            "tunnel":{"phase":"connected","error":null},
            "publication":{"phase":"failed","error":"directory-unreachable"}
        });
        assert!(matches!(
            classify_health(&publication_outage),
            HealthObservation::Healthy(Some("external-connectivity-pending"))
        ));
        let publication_storage_failure = serde_json::json!({
            "connectionState":{"mode":"managed"},
            "tunnel":{"phase":"connected","error":null},
            "publication":{"phase":"failed","error":"storage-unavailable"}
        });
        assert!(matches!(
            classify_health(&publication_storage_failure),
            HealthObservation::LocalFailure
        ));
        let exited_tunnel = serde_json::json!({
            "connectionState":{"mode":"managed"},
            "tunnel":{"phase":"failed","error":"tunnel-exited"},
            "publication":{"phase":"waiting","error":null}
        });
        assert!(matches!(
            classify_health(&exited_tunnel),
            HealthObservation::LocalFailure
        ));
    }

    #[tokio::test]
    async fn held_owner_without_a_locator_is_not_treated_as_stopped_intent() {
        let temp = tempfile::tempdir().unwrap();
        let owner = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(temp.path().join("owner.lock"))
            .unwrap();
        owner.try_lock_exclusive().unwrap();
        let client = Client::new(temp.path().to_owned()).unwrap();
        assert!(matches!(
            daemon_presence(&client, temp.path(), &temp.path().join("server"))
                .await
                .unwrap(),
            DaemonPresence::StartingOrUnreachable
        ));
    }

    #[tokio::test]
    async fn authenticated_status_wins_while_the_live_daemon_holds_owner_lock() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(risunest_sync_server::store::Store::init(temp.path()).unwrap());
        let management = risunest_sync_server::management::Management::start(
            store,
            "127.0.0.1:0".parse().unwrap(),
        )
        .await
        .unwrap();
        let client = Client::new(temp.path().to_owned()).unwrap();
        assert!(matches!(
            daemon_presence(&client, temp.path(), &temp.path().join("server"))
                .await
                .unwrap(),
            DaemonPresence::Running(_)
        ));
        management.close().await;
    }

    fn maintenance_json(revision: &str, state: &str, drained: bool, lease: bool) -> String {
        let mut value = serde_json::json!({
            "revision":revision,"state":state,"idleForSeconds":30,
            "idleThresholdSeconds":30,"idleEligible":true,"activeRequests":0,
            "activeBackgroundJobs":0,"durablePending":{"commitJobs":0,"uploadJobs":0,
            "downloadJobs":0,"stagedChanges":0,"uploads":0},"drained":drained,
            "leaseExpiresInSeconds":if lease {Some(45)} else {None},"leaseDurationSeconds":45
        });
        if lease {
            value["leaseToken"] = serde_json::Value::String("b".repeat(64));
        }
        value.to_string()
    }

    #[tokio::test]
    async fn lost_shutdown_response_is_reconciled_from_process_exit() {
        let temp = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let locator = risunest_sync_server::management::discovery::Discovery {
            address: listener.local_addr().unwrap(),
            token: "a".repeat(64),
        };
        let server = std::thread::spawn(move || {
            let responses = [
                Some(maintenance_json("r0", "open", false, false)),
                Some(maintenance_json("r1", "draining", true, true)),
                Some(maintenance_json("r1", "draining", true, false)),
                None,
            ];
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 8192];
                let read = stream.read(&mut request).unwrap();
                assert!(String::from_utf8_lossy(&request[..read])
                    .to_ascii_lowercase()
                    .contains("authorization: bearer"));
                if let Some(body) = response {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .unwrap();
                }
            }
        });
        let client = Client::new(temp.path().to_owned()).unwrap();
        let mut status = UpdateStatus::default();
        assert!(
            drain_and_shutdown(&client, temp.path(), &locator, &mut status)
                .await
                .unwrap()
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn mid_drain_failure_attempts_to_release_the_maintenance_lease() {
        let temp = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let locator = risunest_sync_server::management::discovery::Discovery {
            address: listener.local_addr().unwrap(),
            token: "a".repeat(64),
        };
        let server = std::thread::spawn(move || {
            let responses = [
                (200, maintenance_json("r0", "open", false, false)),
                (200, maintenance_json("r1", "draining", false, true)),
                (500, r#"{"error":"synthetic-drain-failed"}"#.into()),
                (200, maintenance_json("r1", "draining", false, false)),
                (200, maintenance_json("r2", "open", false, false)),
            ];
            let mut requests = Vec::new();
            for (code, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 8192];
                let read = stream.read(&mut request).unwrap();
                requests.push(String::from_utf8_lossy(&request[..read]).into_owned());
                write!(
                    stream,
                    "HTTP/1.1 {code} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
            requests
        });
        let client = Client::new(temp.path().to_owned()).unwrap();
        let mut status = UpdateStatus::default();
        assert_eq!(
            drain_and_shutdown(&client, temp.path(), &locator, &mut status)
                .await
                .unwrap_err(),
            "synthetic-drain-failed"
        );
        let requests = server.join().unwrap();
        assert!(requests
            .last()
            .unwrap()
            .starts_with("POST /maintenance/release "));
    }

    #[tokio::test]
    async fn losing_the_update_lock_does_not_overwrite_the_winners_status() {
        let temp = tempfile::tempdir().unwrap();
        let _winner = try_lock(temp.path()).unwrap();
        let status = UpdateStatus {
            phase: UpdatePhase::Downloading,
            target_version: Some("2.0.0".into()),
            ..UpdateStatus::default()
        };
        save_status(temp.path(), &status).unwrap();
        assert_eq!(
            run(
                temp.path(),
                &temp.path().join("managed-install/server"),
                RunMode::Manual
            )
            .await
            .unwrap_err(),
            "update-already-running"
        );
        assert_eq!(load_status(temp.path()).unwrap(), status);
    }

    fn helper_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let install = temp.path().join("managed-install");
        let staged = temp.path().join(".risunest-sync-update-stage");
        let backup = temp.path().join(".risunest-sync-update-backup");
        fs::create_dir(&install).unwrap();
        (temp, install, staged, backup)
    }

    #[tokio::test]
    async fn stopped_bundle_is_rolled_back_when_temporary_health_check_cannot_start() {
        let (temp, install, staged, backup) = helper_fixture();
        fs::write(install.join("version"), "old").unwrap();
        fs::create_dir(&staged).unwrap();
        fs::write(staged.join("version"), "new").unwrap();
        let transaction = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Files,
            install.clone(),
            staged,
            backup,
            vec![PathBuf::from("version")],
            false,
        )
        .unwrap();
        transaction.save(temp.path()).unwrap();

        let error = run_helper(
            temp.path(),
            &install.join("risunest-sync-server"),
            4_000_000,
            &install,
        )
        .await
        .unwrap_err();

        assert_eq!(error, "server-executable-or-data-path-invalid");
        assert_eq!(fs::read_to_string(install.join("version")).unwrap(), "old");
        assert_eq!(
            InstallTransaction::load(temp.path(), &install)
                .unwrap()
                .unwrap()
                .phase,
            TransactionPhase::RolledBack
        );
        assert_eq!(load_status(temp.path()).unwrap().phase, UpdatePhase::Failed);
    }

    #[tokio::test]
    async fn helper_restores_the_old_bundle_when_installing_the_stage_fails() {
        let (temp, install, staged, backup) = helper_fixture();
        fs::write(install.join("version"), "old").unwrap();
        let transaction = InstallTransaction::new(
            "1.0.0".into(),
            "2.0.0".into(),
            TransactionKind::Files,
            install.clone(),
            staged,
            backup,
            vec![PathBuf::from("version")],
            false,
        )
        .unwrap();
        transaction.save(temp.path()).unwrap();

        let error = run_helper(
            temp.path(),
            &install.join("risunest-sync-server"),
            4_000_000,
            &install,
        )
        .await
        .unwrap_err();

        assert_eq!(error, "update-package-file-missing");
        assert_eq!(fs::read_to_string(install.join("version")).unwrap(), "old");
        assert_eq!(load_status(temp.path()).unwrap().phase, UpdatePhase::Failed);
    }
}
