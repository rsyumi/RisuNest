#![cfg(target_os = "macos")]

use fs2::FileExt;
use risunest_release_update::{
    Architecture, OperatingSystem, PackageFormat, TrustedPublicKey, VendorArtifact,
};
use risunest_sync_manager::{
    client::Client,
    lifecycle, platform,
    update::{
        extract_package, load_status, try_lock, InstallTransaction, TransactionKind,
        TransactionPhase, UpdatePhase,
    },
};
use risunest_sync_server::{management::discovery::Discovery, PROTOCOL_ID, STORE_FORMAT_ID};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

const APP_NAME: &str = "RisuNest Sync.app";
const SERVER_NAME: &str = "risunest-sync-server";
const MANAGER_NAME: &str = "risunest-sync-manager";

#[cfg(target_arch = "x86_64")]
const TARGET_ARCH: Architecture = Architecture::X86_64;
#[cfg(target_arch = "aarch64")]
const TARGET_ARCH: Architecture = Architecture::Aarch64;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildInventoryFixture {
    schema: String,
    product: String,
    downloads: Vec<BuildDownloadFixture>,
    vendor: Vec<BuildVendorFixture>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildDownloadFixture {
    product: String,
    variant: String,
    os: OperatingSystem,
    arch: Architecture,
    format: PackageFormat,
    version: String,
    file_name: String,
    size: u64,
    sha256: String,
    signature_file_name: String,
    signature: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildVendorFixture {
    name: String,
    version: String,
    os: OperatingSystem,
    arch: Architecture,
    package_arch: Architecture,
    sha256: String,
}

struct VerifiedFixture {
    _temporary: tempfile::TempDir,
    canonical_app: PathBuf,
    version: String,
}

impl VerifiedFixture {
    fn load() -> Result<Self, String> {
        let archive = required_file("RISUNEST_TEST_MAC_APP_ARCHIVE")?;
        let archive_signature_path = required_file("RISUNEST_TEST_MAC_APP_ARCHIVE_SIGNATURE")?;
        let archive_signature = fs::read_to_string(&archive_signature_path)
            .map_err(|error| format!("archive signature read failed: {error}"))?;
        let inventory_path = required_file("RISUNEST_TEST_MAC_BUILD_INVENTORY")?;
        let inventory_bytes =
            fs::read(&inventory_path).map_err(|error| format!("build-inventory-read:{error}"))?;
        let inventory: BuildInventoryFixture = serde_json::from_slice(&inventory_bytes)
            .map_err(|error| format!("build-inventory-invalid:{error}"))?;
        let public_key = std::env::var("RISUNEST_TEST_UPDATE_PUBLIC_KEY")
            .map_err(|_| "RISUNEST_TEST_UPDATE_PUBLIC_KEY is required".to_owned())?;
        TrustedPublicKey::from_tauri(&public_key)
            .map_err(|error| format!("public-key-invalid:{error}"))?
            .verify_file(&archive, &archive_signature)
            .map_err(|error| format!("package-signature-invalid:{error}"))?;
        if inventory.schema != "risunest.release-build/v1" || inventory.product != "sync" {
            return Err("build inventory is not a Sync release leg".into());
        }
        let matching = inventory
            .downloads
            .iter()
            .filter(|download| {
                download.product == "sync"
                    && download.variant == "managed"
                    && download.os == OperatingSystem::Darwin
                    && download.arch == TARGET_ARCH
                    && download.format == PackageFormat::AppTarGz
            })
            .cloned()
            .collect::<Vec<_>>();
        if matching.len() != 1 {
            return Err("build inventory has no unique managed whole-app download".into());
        }
        let download = &matching[0];
        if archive.file_name().and_then(|name| name.to_str()) != Some(download.file_name.as_str())
            || archive_signature_path
                .file_name()
                .and_then(|name| name.to_str())
                != Some(download.signature_file_name.as_str())
            || archive_signature.trim() != download.signature.trim()
            || archive.metadata().map_err(|error| error.to_string())?.len() != download.size
            || file_hash(&archive)? != download.sha256
        {
            return Err("archive fixture does not match its release build inventory".into());
        }
        let expected_vendor = inventory
            .vendor
            .into_iter()
            .filter(|vendor| {
                vendor.os == OperatingSystem::Darwin && vendor.package_arch == TARGET_ARCH
            })
            .map(|vendor| VendorArtifact {
                name: vendor.name,
                version: vendor.version,
                os: vendor.os,
                arch: vendor.arch,
                sha256: vendor.sha256,
            })
            .collect::<Vec<_>>();
        if expected_vendor.len() != 1 {
            return Err("build inventory has no unique target vendor proof".into());
        }

        let temporary = tempfile::Builder::new()
            .prefix("risunest-sync-mac-artifact-")
            .tempdir_in(runner_temp()?)
            .map_err(|error| error.to_string())?;
        let unpack = temporary.path().join("verified-unpack");
        let extracted = extract_package(
            PackageFormat::AppTarGz,
            &archive,
            &unpack,
            &download.version,
            &expected_vendor,
            OperatingSystem::Darwin,
            TARGET_ARCH,
        )
        .map_err(|error| format!("package-extract:{error}"))?;
        verify_codesign(&extracted.root)?;
        for name in [SERVER_NAME, MANAGER_NAME] {
            let binary = extracted.root.join("Contents/MacOS").join(name);
            if !binary.is_file()
                || binary
                    .metadata()
                    .map_err(|error| error.to_string())?
                    .permissions()
                    .mode()
                    & 0o111
                    == 0
            {
                return Err(format!(
                    "packaged executable is missing or not executable: {name}"
                ));
            }
        }
        Ok(Self {
            _temporary: temporary,
            canonical_app: extracted.root,
            version: download.version.clone(),
        })
    }
}

struct Scenario {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    install: PathBuf,
    staged: PathBuf,
    backup: PathBuf,
    server: PathBuf,
    manager: PathBuf,
    data_probe: PathBuf,
    launch_label: String,
    helper_label: String,
}

impl Scenario {
    fn new(fixture: &VerifiedFixture, suffix: &str) -> Result<Self, String> {
        let temporary = tempfile::Builder::new()
            .prefix(&format!("risunest-sync-mac-update-{suffix}-"))
            .tempdir_in(runner_temp()?)
            .map_err(|error| error.to_string())?;
        let base = temporary.path();
        let root = base.join("synthetic-data");
        let bundles = base.join("bundles");
        fs::create_dir_all(&root).map_err(|error| error.to_string())?;
        fs::create_dir_all(&bundles).map_err(|error| error.to_string())?;
        let install = bundles.join(APP_NAME);
        let staged = bundles.join(format!(".risunest-sync-update-stage-{suffix}"));
        let backup = bundles.join(format!(".risunest-sync-update-backup-{suffix}"));
        ditto(&fixture.canonical_app, &install)?;
        ditto(&fixture.canonical_app, &staged)?;
        verify_codesign(&install)?;
        let server = install.join("Contents/MacOS").join(SERVER_NAME);
        let manager = install.join("Contents/MacOS").join(MANAGER_NAME);
        let data_probe = root.join("synthetic-user-data.bin");
        fs::write(&data_probe, b"synthetic-data-must-survive-managed-update")
            .map_err(|error| error.to_string())?;
        let launch_label = format!("io.github.rsyumi.{}", platform::instance_name(&root));
        let helper_label = format!("{launch_label}-apply-{}", std::process::id());
        Ok(Self {
            _temporary: temporary,
            root,
            install,
            staged,
            backup,
            server,
            manager,
            data_probe,
            launch_label,
            helper_label,
        })
    }

    fn install_startup(&self) -> Result<(), String> {
        let status = platform::startup(&self.root, &self.server, "install")?;
        if !status.registered || !status.enabled || !status.action_matches {
            return Err("synthetic LaunchAgent did not reach the expected state".into());
        }
        Ok(())
    }

    async fn stop(&self) -> Result<(), String> {
        lifecycle::stop(&self.root, &Client::new(self.root.clone())?).await
    }

    fn cleanup_launchd(&self) {
        let domain = launchd_domain();
        for label in [&self.helper_label, &self.launch_label] {
            let _ = Command::new("launchctl")
                .args(["bootout", &format!("{domain}/{label}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let _ = Command::new("launchctl")
                .args(["remove", label])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
        let _ = platform::startup(&self.root, &self.server, "remove");
    }

    fn cleanup_launchd_checked(&self) -> Result<(), String> {
        self.cleanup_launchd();
        let domain = launchd_domain();
        for label in [&self.helper_label, &self.launch_label] {
            if Command::new("launchctl")
                .args(["print", &format!("{domain}/{label}")])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|error| format!("launchctl cleanup check failed: {error}"))?
                .success()
            {
                return Err(format!("synthetic launchd job remains loaded: {label}"));
            }
        }
        if platform::startup(&self.root, &self.server, "status")?.registered {
            return Err("synthetic startup plist remains installed".into());
        }
        Ok(())
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        self.cleanup_launchd();
    }
}

fn required_file(name: &str) -> Result<PathBuf, String> {
    let path = std::env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("{name} is required"))?;
    if !path.is_absolute() || !path.is_file() {
        return Err(format!("{name} must name an absolute file"));
    }
    Ok(path)
}

fn runner_temp() -> Result<PathBuf, String> {
    if std::env::var("GITHUB_ACTIONS").as_deref() != Ok("true") {
        return Err("macOS whole-app harness requires an ephemeral GitHub Actions user".into());
    }
    let path = std::env::var_os("RUNNER_TEMP")
        .map(PathBuf::from)
        .ok_or("RUNNER_TEMP is required")?;
    if !path.is_absolute() || !path.is_dir() {
        return Err("RUNNER_TEMP must name an absolute directory".into());
    }
    Ok(path)
}

fn ditto(source: &Path, destination: &Path) -> Result<(), String> {
    let status = Command::new("ditto")
        .arg(source)
        .arg(destination)
        .status()
        .map_err(|error| format!("ditto unavailable: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("ditto failed".into())
    }
}

fn verify_codesign(app: &Path) -> Result<(), String> {
    let status = Command::new("codesign")
        .args(["--verify", "--deep", "--strict"])
        .arg(app)
        .status()
        .map_err(|error| format!("codesign unavailable: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("update-native-signature-invalid".into())
    }
}

fn launchd_domain() -> String {
    let output = Command::new("id").arg("-u").output().unwrap();
    assert!(output.status.success());
    format!("gui/{}", String::from_utf8_lossy(&output.stdout).trim())
}

fn inode(path: &Path) -> Result<u64, String> {
    path.metadata()
        .map(|metadata| metadata.ino())
        .map_err(|error| error.to_string())
}

fn listener_pid(root: &Path) -> Result<u32, String> {
    let discovery = Discovery::load(root).map_err(|error| error.code.to_owned())?;
    let output = Command::new("/usr/sbin/lsof")
        .args([
            "-nP",
            "-a",
            &format!("-iTCP:{}", discovery.address.port()),
            "-sTCP:LISTEN",
            "-t",
        ])
        .output()
        .map_err(|error| format!("lsof unavailable: {error}"))?;
    if !output.status.success() {
        return Err("management listener PID unavailable".into());
    }
    let pids = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .collect::<Vec<_>>();
    if pids.len() != 1 {
        return Err(format!(
            "expected one management listener PID, got {pids:?}"
        ));
    }
    Ok(pids[0])
}

fn process_exited(pid: u32) -> bool {
    if unsafe { libc::kill(pid as i32, 0) } == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

fn file_hash(path: &Path) -> Result<String, String> {
    fs::read(path)
        .map(|bytes| hex::encode(Sha256::digest(bytes)))
        .map_err(|error| error.to_string())
}

async fn wait_healthy(root: &Path, version: &str) -> Result<serde_json::Value, String> {
    let client = Client::new(root.to_owned())?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut last = "no authenticated response".to_owned();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), client.status()).await {
            Ok(Ok(status)) => {
                let publication = status["publication"]["phase"].as_str();
                let local_healthy = status["connectionState"]["mode"] != "managed"
                    && matches!(publication, Some("published" | "disabled" | "stopped"));
                if status["version"] == version
                    && status["protocolId"] == PROTOCOL_ID
                    && status["storeFormatId"] == STORE_FORMAT_ID
                    && local_healthy
                {
                    return Ok(status);
                }
                last = format!("unexpected authenticated status: {status}");
            }
            Ok(Err(error)) => last = error,
            Err(_) => last = "authenticated status timed out".into(),
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("server health timeout: {last}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn helper_lock_active(root: &Path) -> Result<bool, String> {
    let path = root.join("manager-update/helper.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| error.to_string())?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.unlock().map_err(|error| error.to_string())?;
            Ok(false)
        }
        Err(_) => Ok(true),
    }
}

fn sleeping_parent() -> Result<Child, String> {
    Command::new("sh")
        .args(["-c", "sleep 2"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("parent spawn failed: {error}"))
}

async fn spawn_production_helper(scenario: &Scenario, parent: &mut Child) -> Result<(), String> {
    let helper_directory = scenario.root.join("manager-update/helper");
    fs::create_dir_all(&helper_directory).map_err(|error| error.to_string())?;
    let helper = helper_directory.join("risunest-sync-update-helper");
    fs::copy(&scenario.manager, &helper).map_err(|error| format!("helper copy failed: {error}"))?;
    fs::set_permissions(&helper, scenario.manager.metadata().unwrap().permissions())
        .map_err(|error| format!("helper mode failed: {error}"))?;

    let parent_lock = try_lock(&scenario.root)?;
    let mut command = Command::new(&helper);
    command
        .args(["--data-dir"])
        .arg(&scenario.root)
        .args(["--server"])
        .arg(&scenario.server)
        .args(["update", "helper", &parent.id().to_string()])
        .arg(&scenario.install);
    platform::spawn_update_helper(&scenario.root, &mut command)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if helper_lock_active(&scenario.root)? {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("detached helper did not claim its durable lock".into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(parent_lock);
    Ok(())
}

async fn wait_completed(scenario: &Scenario) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        if InstallTransaction::load(&scenario.root, &scenario.install)?.is_none()
            && load_status(&scenario.root)?.phase == UpdatePhase::Completed
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("detached helper completion timeout".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_rolled_back(scenario: &Scenario, target: &str) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(130);
    loop {
        let transaction = InstallTransaction::load(&scenario.root, &scenario.install)?;
        let status = load_status(&scenario.root)?;
        if transaction
            .as_ref()
            .is_some_and(|value| value.phase == TransactionPhase::RolledBack)
            && status.phase == UpdatePhase::Failed
            && status.last_failed_version.as_deref() == Some(target)
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "rollback timeout: transaction={:?}, status={:?}",
                transaction.map(|value| value.phase),
                status.phase
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_restarting(scenario: &Scenario, source_inode: u64) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let transaction = InstallTransaction::load(&scenario.root, &scenario.install)?;
        if transaction
            .as_ref()
            .is_some_and(|value| value.phase == TransactionPhase::Restarting)
            && inode(&scenario.install)? != source_inode
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "target app did not reach post-exchange health check: transaction={:?}",
                transaction.map(|value| value.phase)
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn prepare_transaction(
    scenario: &Scenario,
    source_version: &str,
    target_version: &str,
    was_running: bool,
) -> Result<(), String> {
    InstallTransaction::new(
        source_version.to_owned(),
        target_version.to_owned(),
        TransactionKind::Directory,
        scenario.install.clone(),
        scenario.staged.clone(),
        scenario.backup.clone(),
        Vec::new(),
        was_running,
    )?
    .save(&scenario.root)
}

async fn successful_update(
    fixture: &VerifiedFixture,
    suffix: &str,
    was_running: bool,
) -> Result<(), String> {
    let scenario = Scenario::new(fixture, suffix)?;
    let data_hash = file_hash(&scenario.data_probe)?;
    let source_inode = inode(&scenario.install)?;
    scenario.install_startup()?;
    wait_healthy(&scenario.root, &fixture.version).await?;
    scenario.stop().await?;
    prepare_transaction(&scenario, &fixture.version, &fixture.version, was_running)?;
    let mut parent = sleeping_parent()?;
    spawn_production_helper(&scenario, &mut parent).await?;
    assert_eq!(inode(&scenario.install)?, source_inode);
    parent.wait().map_err(|error| error.to_string())?;
    wait_completed(&scenario).await?;
    if was_running {
        wait_healthy(&scenario.root, &fixture.version).await?;
    } else {
        lifecycle::wait_owner_released(&scenario.root, Duration::from_secs(10)).await?;
        if lifecycle::locator_exists(&scenario.root)? {
            return Err("stopped update left a management locator".into());
        }
    }
    let startup = platform::startup(&scenario.root, &scenario.server, "status")?;
    if !startup.registered || !startup.enabled || !startup.action_matches {
        return Err("update changed the synthetic startup registration".into());
    }
    if inode(&scenario.install)? == source_inode {
        return Err("whole-app directory exchange did not install the staged app".into());
    }
    verify_codesign(&scenario.install)?;
    if scenario.staged.exists() || scenario.backup.exists() {
        return Err("successful update retained transaction directories".into());
    }
    if file_hash(&scenario.data_probe)? != data_hash {
        return Err("synthetic data changed during update".into());
    }
    if was_running {
        scenario.stop().await?;
    }
    scenario.cleanup_launchd_checked()?;
    Ok(())
}

async fn failing_update_rolls_back(fixture: &VerifiedFixture) -> Result<(), String> {
    let scenario = Scenario::new(fixture, "rollback")?;
    let data_hash = file_hash(&scenario.data_probe)?;
    let source_inode = inode(&scenario.install)?;
    scenario.install_startup()?;
    wait_healthy(&scenario.root, &fixture.version).await?;
    scenario.stop().await?;
    let injected_target = format!("{}-synthetic-health-mismatch", fixture.version);
    prepare_transaction(&scenario, &fixture.version, &injected_target, true)?;
    let mut parent = sleeping_parent()?;
    spawn_production_helper(&scenario, &mut parent).await?;
    parent.wait().map_err(|error| error.to_string())?;
    wait_restarting(&scenario, source_inode).await?;
    wait_healthy(&scenario.root, &fixture.version).await?;
    let target_pid = listener_pid(&scenario.root)?;
    wait_rolled_back(&scenario, &injected_target).await?;
    wait_healthy(&scenario.root, &fixture.version).await?;
    let source_pid = listener_pid(&scenario.root)?;
    if source_pid == target_pid || !process_exited(target_pid) {
        return Err(format!(
            "rollback did not replace the target daemon process: target={target_pid}, source={source_pid}"
        ));
    }
    if inode(&scenario.install)? != source_inode {
        return Err("rollback did not atomically restore the source app directory".into());
    }
    verify_codesign(&scenario.install)?;
    let status = load_status(&scenario.root)?;
    if status.reason.as_deref() != Some("updated-server-health-failed") {
        return Err(format!("unexpected rollback reason: {:?}", status.reason));
    }
    let startup = platform::startup(&scenario.root, &scenario.server, "status")?;
    if !startup.registered || !startup.enabled || !startup.action_matches {
        return Err("rollback changed the synthetic startup registration".into());
    }
    if file_hash(&scenario.data_probe)? != data_hash {
        return Err("synthetic data changed during rollback".into());
    }
    scenario.stop().await?;
    scenario.cleanup_launchd_checked()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires final signed macOS release artifacts and a disposable GUI login session"]
async fn produced_whole_app_helper_commits_and_rolls_back() {
    let fixture = VerifiedFixture::load().unwrap();
    successful_update(&fixture, "running", true).await.unwrap();
    successful_update(&fixture, "stopped", false).await.unwrap();
    failing_update_rolls_back(&fixture).await.unwrap();
    eprintln!(
        "verified signed {:?} whole-app update, running/stopped intent, and health rollback",
        TARGET_ARCH
    );
}
