use fs2::FileExt;
use risunest_sync_manager::update::{
    load_status, run_helper, try_lock, InstallTransaction, RunOutcome, TransactionKind,
    TransactionPhase, UpdatePhase,
};
#[cfg(windows)]
use risunest_sync_manager::{client::Client, lifecycle, platform};
#[cfg(windows)]
use risunest_sync_server::{management::discovery::Discovery, PROTOCOL_ID, STORE_FORMAT_ID};
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    install: PathBuf,
    server: PathBuf,
}

fn prepared_file_transaction(was_running: bool) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    let install = root.join("managed-install");
    let staged = root.join(".risunest-sync-update-stage-e2e");
    let backup = root.join(".risunest-sync-update-backup-e2e");
    for directory in [install.join("bin"), staged.join("bin")] {
        fs::create_dir_all(directory).unwrap();
    }
    fs::write(install.join("bin/server"), b"old-server").unwrap();
    fs::write(install.join("bin/obsolete"), b"old-obsolete").unwrap();
    fs::write(install.join("keep-user-file"), b"synthetic-user-owned").unwrap();
    fs::write(staged.join("bin/server"), b"new-server").unwrap();
    fs::write(staged.join("bin/manager"), b"new-manager").unwrap();
    let transaction = InstallTransaction::new_with_removed(
        "1.0.0".into(),
        "2.0.0".into(),
        TransactionKind::Files,
        install.clone(),
        staged,
        backup,
        vec![PathBuf::from("bin/server"), PathBuf::from("bin/manager")],
        vec![PathBuf::from("bin/obsolete")],
        was_running,
    )
    .unwrap();
    transaction.save(&root).unwrap();
    let server = install.join("bin/server");
    Fixture {
        _temp: temp,
        root,
        install,
        server,
    }
}

#[cfg(windows)]
fn sleeping_parent() -> Child {
    Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "Start-Sleep -Seconds 2",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

#[cfg(unix)]
fn sleeping_parent() -> Child {
    Command::new("sh")
        .args(["-c", "sleep 2"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn run_in_thread(
    root: PathBuf,
    server: PathBuf,
    parent: u32,
    install: PathBuf,
) -> mpsc::Receiver<Result<RunOutcome, String>> {
    let (send, receive) = mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = send.send(runtime.block_on(run_helper(&root, &server, parent, &install)));
    });
    receive
}

fn assert_old_install(install: &Path) {
    assert_eq!(fs::read(install.join("bin/server")).unwrap(), b"old-server");
    assert_eq!(
        fs::read(install.join("bin/obsolete")).unwrap(),
        b"old-obsolete"
    );
    assert!(!install.join("bin/manager").exists());
    assert_eq!(
        fs::read(install.join("keep-user-file")).unwrap(),
        b"synthetic-user-owned"
    );
}

#[cfg(windows)]
#[test]
fn stopped_helper_rolls_back_when_the_replacement_cannot_be_started() {
    let fixture = prepared_file_transaction(false);
    let mut parent = sleeping_parent();
    let result = run_in_thread(
        fixture.root.clone(),
        fixture.root.join("missing-server-must-not-start"),
        parent.id(),
        fixture.install.clone(),
    );

    assert!(result.recv_timeout(Duration::from_millis(250)).is_err());
    assert_old_install(&fixture.install);
    let error = result
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap_err();
    let _ = parent.wait();

    assert_eq!(error, "server-executable-or-data-path-invalid");
    assert_old_install(&fixture.install);
    let transaction = InstallTransaction::load(&fixture.root, &fixture.install)
        .unwrap()
        .unwrap();
    assert_eq!(transaction.phase, TransactionPhase::RolledBack);
    let status = load_status(&fixture.root).unwrap();
    assert_eq!(status.phase, UpdatePhase::Failed);
    assert_eq!(
        status.reason.as_deref(),
        Some("server-executable-or-data-path-invalid")
    );
    assert_eq!(status.last_failed_version.as_deref(), Some("2.0.0"));
    assert!(!lifecycle::locator_exists(&fixture.root).unwrap());
    assert!(!lifecycle::owner_active(&fixture.root).unwrap());
}

#[cfg(unix)]
#[test]
fn stopped_directory_helper_rolls_back_when_the_target_cannot_be_health_checked() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().to_owned();
    let install = root.join("managed-install");
    let staged = root.join(".risunest-sync-update-stage-directory");
    let backup = root.join(".risunest-sync-update-backup-directory");
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&staged).unwrap();
    fs::write(install.join("version"), b"old").unwrap();
    fs::write(staged.join("version"), b"new").unwrap();
    InstallTransaction::new(
        "1.0.0".into(),
        "2.0.0".into(),
        TransactionKind::Directory,
        install.clone(),
        staged,
        backup.clone(),
        Vec::new(),
        false,
    )
    .unwrap()
    .save(&root)
    .unwrap();
    let mut parent = sleeping_parent();
    let parent_id = parent.id();
    let (parent_send, parent_receive) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = parent_send.send(parent.wait());
    });
    let result = run_in_thread(
        root.clone(),
        root.join("missing-server-must-not-start"),
        parent_id,
        install.clone(),
    );

    assert!(result.recv_timeout(Duration::from_millis(250)).is_err());
    assert_eq!(fs::read(install.join("version")).unwrap(), b"old");
    let error = result
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap_err();
    assert!(parent_receive
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap()
        .success());

    assert_eq!(error, "server-executable-or-data-path-invalid");
    assert_eq!(fs::read(install.join("version")).unwrap(), b"old");
    assert!(!backup.exists());
    let transaction = InstallTransaction::load(&root, &install).unwrap().unwrap();
    assert_eq!(transaction.phase, TransactionPhase::RolledBack);
    let status = load_status(&root).unwrap();
    assert_eq!(status.phase, UpdatePhase::Failed);
    assert_eq!(
        status.reason.as_deref(),
        Some("server-executable-or-data-path-invalid")
    );
}

#[cfg(windows)]
#[test]
fn helper_waits_for_the_parent_update_lock_handoff_before_replacement() {
    let fixture = prepared_file_transaction(true);
    let lock = try_lock(&fixture.root).unwrap();
    let result = run_in_thread(
        fixture.root.clone(),
        fixture.server.clone(),
        4_000_000,
        fixture.install.clone(),
    );

    assert!(result.recv_timeout(Duration::from_millis(300)).is_err());
    assert_old_install(&fixture.install);
    drop(lock);

    let error = result
        .recv_timeout(Duration::from_secs(30))
        .unwrap()
        .unwrap_err();
    assert_eq!(error, "update-rollback-failed");
    assert_old_install(&fixture.install);
}

#[cfg(windows)]
#[test]
fn helper_waits_for_owner_release_and_restores_files_when_restart_fails() {
    let fixture = prepared_file_transaction(true);
    let owner_path = fixture.root.join("owner.lock");
    let owner = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(owner_path)
        .unwrap();
    owner.try_lock_exclusive().unwrap();
    let result = run_in_thread(
        fixture.root.clone(),
        fixture.server.clone(),
        4_000_000,
        fixture.install.clone(),
    );

    assert!(result.recv_timeout(Duration::from_millis(300)).is_err());
    assert_old_install(&fixture.install);
    owner.unlock().unwrap();

    let error = result
        .recv_timeout(Duration::from_secs(30))
        .unwrap()
        .unwrap_err();
    assert_eq!(error, "update-rollback-failed");
    assert_old_install(&fixture.install);
    let transaction = InstallTransaction::load(&fixture.root, &fixture.install)
        .unwrap()
        .unwrap();
    assert_eq!(transaction.phase, TransactionPhase::RolledBack);
    let status = load_status(&fixture.root).unwrap();
    assert_eq!(status.phase, UpdatePhase::Failed);
    assert_eq!(status.reason.as_deref(), Some("update-rollback-failed"));
    assert_eq!(status.last_failed_version.as_deref(), Some("2.0.0"));
}

#[cfg(windows)]
async fn wait_for_authenticated_status(client: &Client) -> Result<serde_json::Value, String> {
    let mut last_error = "no-response".to_owned();
    for _ in 0..100 {
        match client.status().await {
            Ok(status) => return Ok(status),
            Err(error) => last_error = error,
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(format!("synthetic-live-server-not-ready:{last_error}"))
}

#[cfg(windows)]
fn explicit_test_binaries() -> (PathBuf, PathBuf) {
    let server = PathBuf::from(
        std::env::var_os("RISUNEST_TEST_SERVER").expect("RISUNEST_TEST_SERVER is required"),
    );
    let manager = PathBuf::from(
        std::env::var_os("RISUNEST_TEST_MANAGER").expect("RISUNEST_TEST_MANAGER is required"),
    );
    assert!(server.is_absolute() && server.is_file());
    assert!(manager.is_absolute() && manager.is_file());
    (server, manager)
}

#[cfg(windows)]
fn spawn_initial_server(root: &Path, server: &Path) -> Result<Child, String> {
    platform::initialize(root, server).map_err(|error| format!("initial-server-init:{error}"))?;
    platform::process(server)
        .args(["serve", "--data-dir"])
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "initial-server-spawn-failed".into())
}

#[cfg(windows)]
fn wait_for_initial_server_exit(child: &mut Child) -> Result<(), String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if child
            .try_wait()
            .map_err(|_| "initial-server-exit-check-failed")?
            .is_some()
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("initial-server-exit-timeout".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(windows)]
fn scheduled_server_diagnostic(root: &Path, server: &Path) -> serde_json::Value {
    const SNAPSHOT: &str = r#"$ErrorActionPreference='Stop';$service=New-Object -ComObject 'Schedule.Service';$service.Connect();$folder=$service.GetFolder('\');try{$task=$folder.GetTask($env:RISUNEST_TEST_TASK_NAME);@{present=$true;state=[int]$task.State;lastResult=[int64]$task.LastTaskResult}|ConvertTo-Json -Compress}catch{@{present=$false;state=$null;lastResult=$null}|ConvertTo-Json -Compress}"#;
    let task = platform::process("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SNAPSHOT,
        ])
        .env("RISUNEST_TEST_TASK_NAME", platform::instance_name(root))
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice(&output.stdout).ok())
        .unwrap_or_else(|| serde_json::json!({"error":"task-snapshot-unavailable"}));
    let startup = platform::startup(root, server, "status")
        .map(|status| {
            serde_json::json!({
                "registered": status.registered,
                "enabled": status.enabled,
                "actionMatches": status.action_matches,
            })
        })
        .unwrap_or_else(|error| serde_json::json!({"error":error}));
    let discovery = match Discovery::load(root) {
        Ok(_) => "valid",
        Err(error) => error.code,
    };
    serde_json::json!({
        "task": task,
        "startup": startup,
        "locator": lifecycle::locator_exists(root).unwrap_or(false),
        "owner": lifecycle::owner_active(root).unwrap_or(false),
        "discovery": discovery,
    })
}

#[cfg(windows)]
fn write_installed_inventory(install: &Path, files: &[&std::ffi::OsStr]) {
    let files = files
        .iter()
        .map(|name| name.to_string_lossy())
        .collect::<Vec<_>>();
    fs::write(
        install.join("risunest-sync-bundle.json"),
        serde_json::to_vec(&serde_json::json!({
            "schema": "risunest-sync-bundle/v1",
            "product": "sync",
            "variant": "managed",
            "version": env!("CARGO_PKG_VERSION"),
            "protocolId": PROTOCOL_ID,
            "storeFormatId": STORE_FORMAT_ID,
            "files": files,
            "vendor": [],
        }))
        .unwrap(),
    )
    .unwrap();
}

#[cfg(windows)]
const SCHEDULED_HARNESS_SCRIPT: &str = r#"
$ErrorActionPreference='Stop'
$service=New-Object -ComObject 'Schedule.Service'; $service.Connect(); $folder=$service.GetFolder('\')
if($env:RISUNEST_TEST_TASK_ACTION -eq 'remove') {
 try {$folder.DeleteTask($env:RISUNEST_TEST_TASK_NAME,0)} catch {
  $reason=$_.Exception; while($reason.InnerException){$reason=$reason.InnerException}
  if($reason.HResult -ne -2147024894 -and $reason.HResult -ne -2147024893){throw}
 }
 exit 0
}
$sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
$definition=$service.NewTask(0)
$definition.RegistrationInfo.Description='RisuNest synthetic detached helper test'
$definition.Principal.UserId=$sid
$definition.Principal.LogonType=3
$definition.Principal.RunLevel=0
$definition.Settings.AllowDemandStart=$true
$definition.Settings.Enabled=$true
$definition.Settings.ExecutionTimeLimit='PT1M'
$action=$definition.Actions.Create(0)
$action.Path='powershell.exe'
$action.Arguments=$env:RISUNEST_TEST_TASK_ARGUMENTS
$task=$folder.RegisterTaskDefinition($env:RISUNEST_TEST_TASK_NAME,$definition,6,$sid,$null,3,$null)
$null=$task.Run($null)
"#;

#[cfg(windows)]
fn scheduled_harness_action(name: &str, action: &str, arguments: &str) -> Result<(), String> {
    let output = platform::process("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCHEDULED_HARNESS_SCRIPT,
        ])
        .env("RISUNEST_TEST_TASK_NAME", name)
        .env("RISUNEST_TEST_TASK_ACTION", action)
        .env("RISUNEST_TEST_TASK_ARGUMENTS", arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "scheduled-harness-unavailable")?;
    if output.status.success() {
        Ok(())
    } else {
        Err("scheduled-harness-operation-failed".into())
    }
}

#[cfg(windows)]
fn remove_transient_helper_tasks(root: &Path) -> Result<(), String> {
    const CLEAN: &str = r#"$ErrorActionPreference='Stop';$service=New-Object -ComObject 'Schedule.Service';$service.Connect();$folder=$service.GetFolder('\');foreach($task in $folder.GetTasks(1)){if($task.Name.StartsWith($env:RISUNEST_TEST_TASK_PREFIX,[StringComparison]::Ordinal)){if($task.Definition.RegistrationInfo.Description -ne 'RisuNest update helper'){throw 'unexpected helper task ownership'};$folder.DeleteTask($task.Name,0)}}"#;
    let output = platform::process("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            CLEAN,
        ])
        .env(
            "RISUNEST_TEST_TASK_PREFIX",
            format!("{}-update-helper-", platform::instance_name(root)),
        )
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "transient-helper-task-cleanup-unavailable")?;
    if output.status.success() {
        Ok(())
    } else {
        Err("transient-helper-task-cleanup-failed".into())
    }
}

#[cfg(windows)]
fn transient_helper_task_count(root: &Path) -> Result<usize, String> {
    const COUNT: &str = r#"$ErrorActionPreference='Stop';$service=New-Object -ComObject 'Schedule.Service';$service.Connect();$folder=$service.GetFolder('\');$count=0;foreach($task in $folder.GetTasks(1)){if($task.Name.StartsWith($env:RISUNEST_TEST_TASK_PREFIX,[StringComparison]::Ordinal)){if($task.Definition.RegistrationInfo.Description -ne 'RisuNest update helper'){throw 'unexpected helper task ownership'};$count++}};[Console]::Out.Write($count)"#;
    let output = platform::process("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            COUNT,
        ])
        .env(
            "RISUNEST_TEST_TASK_PREFIX",
            format!("{}-update-helper-", platform::instance_name(root)),
        )
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "transient-helper-task-status-unavailable")?;
    if !output.status.success() {
        return Err("transient-helper-task-status-failed".into());
    }
    String::from_utf8(output.stdout)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .ok_or("transient-helper-task-status-invalid".into())
}

#[cfg(windows)]
fn helper_lock_active(root: &Path) -> Result<bool, String> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("manager-update/helper.lock"))
        .map_err(|_| "helper-lock-open-failed")?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            file.unlock().map_err(|_| "helper-lock-release-failed")?;
            Ok(false)
        }
        Err(_) => Ok(true),
    }
}

#[cfg(windows)]
fn helper_failure_diagnostic(root: &Path) -> serde_json::Value {
    const TASKS: &str = r#"$ErrorActionPreference='Stop';$service=New-Object -ComObject 'Schedule.Service';$service.Connect();$folder=$service.GetFolder('\');$items=@();foreach($task in $folder.GetTasks(1)){if($task.Name.StartsWith($env:RISUNEST_TEST_TASK_PREFIX,[StringComparison]::Ordinal)){try{$action=$task.Definition.Actions.Item(1);$instances=@();foreach($instance in $task.GetInstances(0)){$instances+=@{enginePid=[int64]$instance.EnginePID;state=[int]$instance.State}};$items+=@{name=$task.Name;description=$task.Definition.RegistrationInfo.Description;program=$action.Path;arguments=$action.Arguments;state=[int]$task.State;lastResult=[int64]$task.LastTaskResult;lastRunTime=$task.LastRunTime.ToString('o');nextRunTime=$task.NextRunTime.ToString('o');instances=$instances}}catch{$items+=@{name=$task.Name;snapshotError='task-disappeared-during-snapshot'}}}};ConvertTo-Json -Compress -InputObject @($items)"#;
    const PROCESSES: &str = r#"$ErrorActionPreference='Stop';$items=@(Get-CimInstance Win32_Process | Where-Object {$null -ne $_.CommandLine -and $_.CommandLine.Contains($env:RISUNEST_TEST_ROOT,[StringComparison]::OrdinalIgnoreCase)} | ForEach-Object {@{pid=[int64]$_.ProcessId;name=$_.Name;commandLine=$_.CommandLine}});ConvertTo-Json -Compress -InputObject $items"#;
    fn powershell_json(
        script: &str,
        name: &str,
        value: impl AsRef<std::ffi::OsStr>,
    ) -> serde_json::Value {
        platform::process("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .env(name, value)
            .stdin(Stdio::null())
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| serde_json::from_slice(&output.stdout).ok())
            .unwrap_or_else(|| serde_json::json!({"error":"snapshot-unavailable"}))
    }
    fn lock_state(path: PathBuf) -> serde_json::Value {
        let existed = path.exists();
        match OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
        {
            Ok(file) => match file.try_lock_exclusive() {
                Ok(()) => {
                    let _ = file.unlock();
                    serde_json::json!({"existed":existed,"exclusiveAvailable":true})
                }
                Err(error) => serde_json::json!({
                    "existed": existed,
                    "exclusiveAvailable": false,
                    "errorKind": format!("{:?}", error.kind()),
                }),
            },
            Err(error) => serde_json::json!({
                "existed": existed,
                "openErrorKind": format!("{:?}", error.kind()),
            }),
        }
    }
    let prefix = format!("{}-update-helper-", platform::instance_name(root));
    let harness_log = fs::read_to_string(root.join("spawn-helper-log"))
        .map(|value| value.chars().take(2_000).collect::<String>())
        .unwrap_or_else(|_| "unavailable".into());
    serde_json::json!({
        "root": root,
        "tasks": powershell_json(TASKS, "RISUNEST_TEST_TASK_PREFIX", prefix),
        "processes": powershell_json(PROCESSES, "RISUNEST_TEST_ROOT", root),
        "helperLock": lock_state(root.join("manager-update/helper.lock")),
        "updateLock": lock_state(root.join("manager-update/instance.lock")),
        "harnessLog": harness_log,
    })
}

#[cfg(windows)]
async fn wait_for_transient_helper_cleanup(root: &Path) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let last_error = match transient_helper_task_count(root) {
            Ok(0) => return Ok(()),
            Ok(_) => None,
            Err(error) => Some(error),
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(last_error.unwrap_or_else(|| "transient-helper-task-remains".into()));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(windows)]
async fn spawn_helper_from_scheduled_harness(
    root: &Path,
    install: &Path,
    server: &Path,
    manager: &Path,
    mode: &str,
) -> Result<(), String> {
    let wrapper = root.join("spawn-helper-harness.ps1");
    let config = root.join("spawn-helper-harness.json");
    let result = root.join("spawn-helper-result");
    let log = root.join("spawn-helper-log");
    fs::write(
        &wrapper,
        r#"param([string]$ConfigPath)
$ErrorActionPreference='Stop'
try {
 $config=Get-Content -Raw -LiteralPath $ConfigPath | ConvertFrom-Json
 $logPath=$config.log
 $resultPath=$config.result
 $env:RISUNEST_HELPER_SUBPROCESS='1'
 $env:RISUNEST_HELPER_ROOT=$config.root
 $env:RISUNEST_HELPER_INSTALL=$config.install
 $env:RISUNEST_HELPER_SERVER=$config.server
 $env:RISUNEST_HELPER_MANAGER=$config.manager
 $env:RISUNEST_HELPER_MODE=$config.mode
 & $config.testExe --ignored --exact spawn_update_helper_subprocess *> $logPath
 if($LASTEXITCODE -ne 0){throw 'helper subprocess failed'}
 [IO.File]::WriteAllText($resultPath,'success')
} catch {
 if(!(Test-Path -LiteralPath $logPath)){[IO.File]::WriteAllText($logPath,$_.Exception.ToString())}
 [IO.File]::WriteAllText($resultPath,'failure')
 exit 1
}
"#,
    )
    .map_err(|_| "scheduled-harness-write-failed")?;
    let test_exe = std::env::current_exe().map_err(|_| "test-executable-missing")?;
    let config_value = serde_json::json!({
        "root": root,
        "install": install,
        "server": server,
        "manager": manager,
        "mode": mode,
        "testExe": test_exe,
        "result": result,
        "log": log,
    });
    fs::write(
        &config,
        serde_json::to_vec(&config_value).map_err(|_| "scheduled-harness-write-failed")?,
    )
    .map_err(|_| "scheduled-harness-write-failed")?;
    if [&wrapper, &config]
        .iter()
        .any(|path| path.to_string_lossy().contains('"'))
    {
        return Err("scheduled-harness-path-invalid".into());
    }
    let task_name = format!("{}-helper-test", platform::instance_name(root));
    let arguments = format!(
        "-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass -File \"{}\" -ConfigPath \"{}\"",
        wrapper.display(),
        config.display()
    );
    let launched = scheduled_harness_action(&task_name, "run", &arguments);
    let wait: Result<(), String> = async {
        launched?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(value) = fs::read_to_string(&result) {
                return if value == "success" {
                    Ok(())
                } else {
                    let detail = fs::read(&log)
                        .map(|bytes| {
                            if bytes.starts_with(&[0xff, 0xfe]) {
                                String::from_utf16_lossy(
                                    &bytes[2..]
                                        .chunks_exact(2)
                                        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                                        .collect::<Vec<_>>(),
                                )
                            } else {
                                String::from_utf8_lossy(&bytes).into_owned()
                            }
                        })
                        .unwrap_or_else(|_| "subprocess log unavailable".into());
                    Err(format!(
                        "scheduled-helper-subprocess-failed:{}",
                        detail.chars().take(2_000).collect::<String>()
                    ))
                };
            }
            if tokio::time::Instant::now() >= deadline {
                return Err("scheduled-helper-subprocess-timeout".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    .await;
    let removed = scheduled_harness_action(&task_name, "remove", "");
    let result = wait.and(removed);
    if result.is_err() {
        let _ = remove_transient_helper_tasks(root);
    }
    result
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn live_helper_replacement(
    root: &Path,
    install: &Path,
    staged: &Path,
    backup: &Path,
    server: &Path,
    manager: &Path,
    files: Vec<PathBuf>,
    was_running: bool,
) -> Result<(), String> {
    let client = Client::new(root.to_owned()).map_err(|error| format!("client-create:{error}"))?;
    let mut initial_server = None;
    if was_running {
        platform::startup(root, server, "install")
            .map_err(|error| format!("startup-install:{error}"))?;
        initial_server = Some(spawn_initial_server(root, server)?);
        let before = match wait_for_authenticated_status(&client).await {
            Ok(status) => status,
            Err(error) => {
                return Err(format!(
                    "initial-health:{error}:{}",
                    scheduled_server_diagnostic(root, server)
                ))
            }
        };
        if before["version"] != env!("CARGO_PKG_VERSION")
            || before["protocolId"] != PROTOCOL_ID
            || before["storeFormatId"] != STORE_FORMAT_ID
        {
            return Err("synthetic-live-server-identity-mismatch".into());
        }
    }

    let transaction = InstallTransaction::new(
        "0.0.0-synthetic-source".into(),
        env!("CARGO_PKG_VERSION").into(),
        TransactionKind::Files,
        install.to_owned(),
        staged.to_owned(),
        backup.to_owned(),
        files,
        was_running,
    )
    .map_err(|error| format!("transaction-create:{error}"))?;
    transaction
        .save(root)
        .map_err(|error| format!("transaction-save:{error}"))?;
    if was_running {
        Discovery::load(root).map_err(|error| format!("pre-stop-discovery:{}", error.code))?;
        client
            .status()
            .await
            .map_err(|error| format!("pre-stop-health:{error}"))?;
        lifecycle::stop(root, &client)
            .await
            .map_err(|error| format!("lifecycle-stop:{error}"))?;
        wait_for_initial_server_exit(initial_server.as_mut().unwrap())?;
    }
    spawn_helper_from_scheduled_harness(root, install, server, manager, "apply-helper").await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let transaction = InstallTransaction::load(root, install)
            .map_err(|error| format!("transaction-load:{error}"))?;
        let status = load_status(root).map_err(|error| format!("status-load:{error}"))?;
        let completed = transaction.is_none() && status.phase == UpdatePhase::Completed;
        if completed {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "detached-helper-completion-timeout:transactionPhase={:?},statusPhase={:?},reason={:?},diagnostic={}",
                transaction.map(|value| value.phase), status.phase, status.reason,
                helper_failure_diagnostic(root)
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if was_running {
        let after = wait_for_authenticated_status(&client)
            .await
            .map_err(|error| format!("restart-health:{error}"))?;
        if after["version"] != env!("CARGO_PKG_VERSION")
            || after["protocolId"] != PROTOCOL_ID
            || after["storeFormatId"] != STORE_FORMAT_ID
        {
            return Err("synthetic-live-server-restart-mismatch".into());
        }
    } else {
        let startup = platform::startup(root, server, "status")
            .map_err(|error| format!("stopped-startup-status:{error}"))?;
        if startup.registered
            || startup.enabled
            || lifecycle::locator_exists(root)?
            || lifecycle::owner_active(root)?
        {
            return Err("synthetic-stopped-intent-not-restored".into());
        }
    }
    if InstallTransaction::load(root, install)
        .map_err(|error| format!("transaction-load:{error}"))?
        .is_some()
        || staged.exists()
        || backup.exists()
        || load_status(root)
            .map_err(|error| format!("status-load:{error}"))?
            .phase
            != UpdatePhase::Completed
    {
        return Err("synthetic-live-helper-cleanup-incomplete".into());
    }
    wait_for_transient_helper_cleanup(root).await?;
    Ok(())
}

#[cfg(windows)]
async fn run_live_replacement_case(was_running: bool) {
    let (source_server, source_manager) = explicit_test_binaries();

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("synthetic-data");
    let install = temp.path().join("managed-install");
    let staged = temp.path().join(".risunest-sync-update-stage-live");
    let backup = temp.path().join(".risunest-sync-update-backup-live");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&staged).unwrap();
    let server_name = source_server.file_name().unwrap();
    let manager_name = source_manager.file_name().unwrap();
    let installed_server = install.join(server_name);
    let installed_manager = install.join(manager_name);
    for (source, name) in [
        (&source_server, server_name),
        (&source_manager, manager_name),
    ] {
        fs::copy(source, install.join(name)).unwrap();
        fs::copy(source, staged.join(name)).unwrap();
    }
    let replacement_proof = PathBuf::from("replacement-proof.txt");
    fs::write(install.join(&replacement_proof), b"old").unwrap();
    fs::write(staged.join(&replacement_proof), b"new").unwrap();
    let files = vec![
        PathBuf::from(server_name),
        PathBuf::from(manager_name),
        replacement_proof.clone(),
    ];

    let result = live_helper_replacement(
        &root,
        &install,
        &staged,
        &backup,
        &installed_server,
        &installed_manager,
        files,
        was_running,
    )
    .await;
    let client = Client::new(root.clone()).unwrap();
    let stop = lifecycle::stop(&root, &client).await;
    let unregister = platform::startup(&root, &installed_server, "remove");
    let transient_cleanup = remove_transient_helper_tasks(&root);

    result.unwrap_or_else(|error| {
        panic!(
            "live helper failed at {error}; cleanup stop={stop:?}, unregister={unregister:?}, transient={transient_cleanup:?}"
        )
    });
    assert_eq!(fs::read(install.join(replacement_proof)).unwrap(), b"new");
    stop.unwrap();
    unregister.unwrap();
    transient_cleanup.unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit synthetic server/manager binaries and exclusive port 4319"]
async fn helper_replaces_and_restarts_an_isolated_live_server() {
    run_live_replacement_case(true).await;
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit synthetic server/manager binaries and exclusive port 4319"]
async fn helper_verifies_a_stopped_replacement_then_restores_stopped_intent() {
    run_live_replacement_case(false).await;
}

#[cfg(windows)]
async fn live_installer_guard_restart_failure(
    root: &Path,
    server: &Path,
    manager: &Path,
) -> Result<(), String> {
    platform::initialize(root, server).map_err(|error| format!("source-init:{error}"))?;
    let mut source_process = platform::process(server)
        .args(["serve", "--data-dir"])
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "source-server-start-failed")?;
    let client = Client::new(root.to_owned()).map_err(|error| format!("client-create:{error}"))?;
    if let Err(error) = wait_for_authenticated_status(&client).await {
        return Err(format!(
            "initial-health:{error}:{}",
            scheduled_server_diagnostic(root, server)
        ));
    }

    let nonce_output = root.join("installer-guard-nonce");
    let nonce_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&nonce_output)
        .map_err(|_| "installer-guard-nonce-file-unavailable")?;
    let prepare = platform::process(manager)
        .args(["--data-dir"])
        .arg(root)
        .args(["--server"])
        .arg(server)
        .args(["installer", "prepare"])
        .stdin(Stdio::null())
        .stdout(Stdio::from(nonce_file))
        .stderr(Stdio::null())
        .status()
        .map_err(|_| "installer-guard-prepare-process-failed")?;
    if !prepare.success() {
        return Err("installer-guard-prepare-cli-failed".into());
    }
    let nonce = fs::read_to_string(nonce_output)
        .map_err(|_| "installer-guard-nonce-invalid")?
        .trim()
        .to_owned();
    if nonce.len() != 64
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("installer-guard-nonce-invalid".into());
    }
    let guard_dir = root.join("manager-update");
    let ready = guard_dir.join(format!("installer-{nonce}.ready"));
    let cancel = guard_dir.join(format!("installer-{nonce}.cancel"));
    let error_path = guard_dir.join(format!("installer-{nonce}.error"));
    let ready_value: serde_json::Value =
        serde_json::from_slice(&fs::read(&ready).map_err(|_| "installer-guard-ready-missing")?)
            .map_err(|_| "installer-guard-ready-invalid")?;
    if ready_value["ready"] != true
        || ready_value["wasRunning"] != true
        || lifecycle::owner_active(root)?
        || lifecycle::locator_exists(root)?
    {
        return Err("installer-guard-did-not-stop-running-server".into());
    }
    let source_exit_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if source_process
            .try_wait()
            .map_err(|_| "source-server-exit-check-failed")?
            .is_some()
        {
            break;
        }
        if std::time::Instant::now() >= source_exit_deadline {
            return Err("installer-guard-source-server-did-not-exit".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    fs::write(server, b"synthetic-corrupt-installer-target")
        .map_err(|_| "installer-guard-corrupt-target-write-failed")?;
    fs::write(&cancel, br#"{"cancel":true}"#).map_err(|_| "installer-guard-cancel-write-failed")?;
    let health_wait_started = std::time::Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(125);
    let error_value = loop {
        if let Ok(bytes) = fs::read(&error_path) {
            break serde_json::from_slice::<serde_json::Value>(&bytes)
                .map_err(|_| "installer-guard-error-invalid")?;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("installer-guard-restart-failure-timeout".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    if health_wait_started.elapsed() < Duration::from_secs(95) {
        return Err("installer-guard-restart-was-not-accepted-for-health-check".into());
    }
    if error_value["error"] != "installer-guard-cancelled"
        || error_value["wasRunning"] != true
        || error_value["restartFailed"] != true
        || ready.exists()
        || cancel.exists()
    {
        return Err(format!(
            "installer-guard-restart-failure-state-mismatch:{error_value}"
        ));
    }
    let startup = platform::startup(root, server, "status")
        .map_err(|error| format!("startup-status:{error}"))?;
    if startup.registered
        || startup.enabled
        || !startup.action_matches
        || lifecycle::owner_active(root)?
        || lifecycle::locator_exists(root)?
    {
        return Err("installer-guard-temporary-start-registration-remains".into());
    }
    if fs::read(root.join("synthetic-preservation-marker"))
        .map_err(|_| "installer-guard-preservation-marker-missing")?
        != b"synthetic-original-data"
        || !root.join("metadata.sqlite").is_file()
    {
        return Err("installer-guard-original-data-changed".into());
    }

    let lock_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match try_lock(root) {
            Ok(lock) => {
                drop(lock);
                break;
            }
            Err(error) if error == "update-already-running" => {
                if tokio::time::Instant::now() >= lock_deadline {
                    return Err("installer-guard-process-did-not-exit".into());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(format!("installer-guard-lock-check:{error}")),
        }
    }
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit synthetic binaries, exclusive port 4319, and the real 100 second health timeout"]
async fn installer_guard_records_restart_failure_after_accepted_corrupt_server_start() {
    let (source_server, source_manager) = explicit_test_binaries();
    let expected_server = fs::read(&source_server).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("synthetic-data");
    let install = temp.path().join("managed-install");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&install).unwrap();
    let server_name = source_server.file_name().unwrap();
    let manager_name = source_manager.file_name().unwrap();
    let installed_server = install.join(server_name);
    fs::write(&installed_server, &expected_server).unwrap();
    fs::copy(&source_manager, install.join(manager_name)).unwrap();
    write_installed_inventory(&install, &[server_name, manager_name]);
    fs::write(
        root.join("synthetic-preservation-marker"),
        b"synthetic-original-data",
    )
    .unwrap();

    let result =
        live_installer_guard_restart_failure(&root, &installed_server, &source_manager).await;
    let client = Client::new(root.clone()).unwrap();
    let stop = lifecycle::stop(&root, &client).await;
    let restore = fs::write(&installed_server, &expected_server);
    let unregister = platform::startup(&root, &installed_server, "remove");

    result.unwrap_or_else(|error| {
        panic!(
            "live installer-guard restart failure test failed at {error}; cleanup stop={stop:?}, restore={restore:?}, unregister={unregister:?}"
        )
    });
    stop.unwrap();
    restore.unwrap();
    unregister.unwrap();
}

#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn live_corrupt_replacement_rollback(
    root: &Path,
    install: &Path,
    staged: &Path,
    backup: &Path,
    server: &Path,
    source_server: &Path,
    source_manager: &Path,
    expected_server: &[u8],
    expected_manager: &[u8],
) -> Result<(), String> {
    platform::startup(root, server, "install")
        .map_err(|error| format!("startup-install:{error}"))?;
    platform::start(root, server).map_err(|error| format!("startup-start:{error}"))?;
    let client = Client::new(root.to_owned()).map_err(|error| format!("client-create:{error}"))?;
    wait_for_authenticated_status(&client)
        .await
        .map_err(|error| format!("initial-health:{error}"))?;
    let server_name = source_server.file_name().unwrap();
    let manager_name = source_manager.file_name().unwrap();
    let target = "99.0.0-synthetic-corrupt-target";
    InstallTransaction::new(
        env!("CARGO_PKG_VERSION").into(),
        target.into(),
        TransactionKind::Files,
        install.to_owned(),
        staged.to_owned(),
        backup.to_owned(),
        vec![PathBuf::from(server_name), PathBuf::from(manager_name)],
        true,
    )
    .map_err(|error| format!("transaction-create:{error}"))?
    .save(root)
    .map_err(|error| format!("transaction-save:{error}"))?;
    lifecycle::stop(root, &client)
        .await
        .map_err(|error| format!("lifecycle-stop:{error}"))?;

    let error = run_helper(root, server, 4_000_000, install)
        .await
        .expect_err("the corrupt target must fail local health verification");
    if error != "updated-server-health-failed" {
        return Err(format!("corrupt-target-error:{error}"));
    }
    let restored = wait_for_authenticated_status(&client)
        .await
        .map_err(|error| format!("rollback-health:{error}"))?;
    if restored["version"] != env!("CARGO_PKG_VERSION")
        || restored["protocolId"] != PROTOCOL_ID
        || restored["storeFormatId"] != STORE_FORMAT_ID
    {
        return Err("corrupt-target-rollback-identity-mismatch".into());
    }
    if fs::read(install.join(server_name)).map_err(|_| "restored-server-read-failed")?
        != expected_server
    {
        return Err("corrupt-target-server-not-restored".into());
    }
    if fs::read(install.join(manager_name)).map_err(|_| "restored-manager-read-failed")?
        != expected_manager
    {
        return Err("corrupt-target-manager-not-restored".into());
    }
    let transaction = InstallTransaction::load(root, install)
        .map_err(|error| format!("transaction-load:{error}"))?
        .ok_or("rollback-transaction-missing")?;
    let status = load_status(root).map_err(|error| format!("status-load:{error}"))?;
    if transaction.phase != TransactionPhase::RolledBack
        || status.phase != UpdatePhase::Failed
        || status.reason.as_deref() != Some("updated-server-health-failed")
        || status.last_failed_version.as_deref() != Some(target)
    {
        return Err("corrupt-target-rollback-status-mismatch".into());
    }
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit synthetic binaries, exclusive port 4319, and the real 100 second health timeout"]
async fn helper_rolls_back_a_corrupt_live_replacement_and_restarts_the_old_server() {
    let (source_server, source_manager) = explicit_test_binaries();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("synthetic-data");
    let install = temp.path().join("managed-install");
    let staged = temp.path().join(".risunest-sync-update-stage-corrupt-live");
    let backup = temp
        .path()
        .join(".risunest-sync-update-backup-corrupt-live");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&staged).unwrap();
    let server_name = source_server.file_name().unwrap();
    let manager_name = source_manager.file_name().unwrap();
    let installed_server = install.join(server_name);
    let expected_server = fs::read(&source_server).unwrap();
    let expected_manager = fs::read(&source_manager).unwrap();
    fs::write(&installed_server, &expected_server).unwrap();
    fs::write(install.join(manager_name), &expected_manager).unwrap();
    fs::write(staged.join(server_name), b"synthetic-corrupt-executable").unwrap();
    fs::write(staged.join(manager_name), &expected_manager).unwrap();

    let result = live_corrupt_replacement_rollback(
        &root,
        &install,
        &staged,
        &backup,
        &installed_server,
        &source_server,
        &source_manager,
        &expected_server,
        &expected_manager,
    )
    .await;
    let client = Client::new(root.clone()).unwrap();
    let stop = lifecycle::stop(&root, &client).await;
    let unregister = platform::startup(&root, &installed_server, "remove");

    result.unwrap_or_else(|error| {
        panic!(
            "live corrupt-target rollback failed at {error}; cleanup stop={stop:?}, unregister={unregister:?}"
        )
    });
    stop.unwrap();
    unregister.unwrap();
}

#[cfg(windows)]
#[test]
#[ignore = "internal subprocess for the live detached-helper test"]
fn spawn_update_helper_subprocess() {
    if std::env::var_os("RISUNEST_HELPER_SUBPROCESS").is_none() {
        return;
    }
    let root = PathBuf::from(std::env::var_os("RISUNEST_HELPER_ROOT").unwrap());
    let install = PathBuf::from(std::env::var_os("RISUNEST_HELPER_INSTALL").unwrap());
    let server = PathBuf::from(std::env::var_os("RISUNEST_HELPER_SERVER").unwrap());
    let manager = PathBuf::from(std::env::var_os("RISUNEST_HELPER_MANAGER").unwrap());
    if std::env::var("RISUNEST_HELPER_MODE").as_deref() == Ok("recover-manager") {
        assert!(Command::new(manager)
            .args(["--data-dir"])
            .arg(&root)
            .args(["--server"])
            .arg(&server)
            .args(["update", "check"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
        return;
    }
    let helper_dir = root.join("manager-update/helper");
    fs::create_dir_all(&helper_dir).unwrap();
    let helper = helper_dir.join("risunest-sync-update-helper.exe");
    fs::copy(manager, &helper).unwrap();
    let parent_lock = try_lock(&root).unwrap();
    let mut command = Command::new(helper);
    command
        .args(["--data-dir"])
        .arg(&root)
        .args(["--server"])
        .arg(&server)
        .args(["update", "helper", &std::process::id().to_string()])
        .arg(&install);
    assert_ne!(
        platform::spawn_update_helper(&root, &mut command).unwrap(),
        0
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if helper_lock_active(&root).unwrap() {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "detached helper did not claim helper.lock"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    drop(parent_lock);
}

#[cfg(windows)]
async fn live_recovery_from_installed_manager(
    root: &Path,
    install: &Path,
    staged: &Path,
    backup: &Path,
    server: &Path,
    manager: &Path,
) -> Result<(), String> {
    platform::startup(root, server, "install")
        .map_err(|error| format!("startup-install:{error}"))?;
    platform::start(root, server).map_err(|error| format!("startup-start:{error}"))?;
    let client = Client::new(root.to_owned()).map_err(|error| format!("client-create:{error}"))?;
    if let Err(error) = wait_for_authenticated_status(&client).await {
        return Err(format!(
            "initial-health:{error}:{}",
            scheduled_server_diagnostic(root, server)
        ));
    }
    let marker = PathBuf::from("recovery-proof.txt");
    let target = "99.0.0-synthetic-interrupted-target";
    let mut transaction = InstallTransaction::new(
        env!("CARGO_PKG_VERSION").into(),
        target.into(),
        TransactionKind::Files,
        install.to_owned(),
        staged.to_owned(),
        backup.to_owned(),
        vec![
            PathBuf::from(server.file_name().unwrap()),
            PathBuf::from(manager.file_name().unwrap()),
            marker.clone(),
        ],
        true,
    )
    .map_err(|error| format!("transaction-create:{error}"))?;
    transaction
        .save(root)
        .map_err(|error| format!("transaction-save:{error}"))?;
    lifecycle::stop(root, &client)
        .await
        .map_err(|error| format!("lifecycle-stop:{error}"))?;
    transaction
        .apply(root)
        .map_err(|error| format!("transaction-apply:{error}"))?;
    transaction
        .mark_restarting(root)
        .map_err(|error| format!("transaction-restarting:{error}"))?;
    if fs::read(install.join(&marker)).map_err(|_| "installed-marker-read-failed")? != b"new" {
        return Err("interrupted-target-was-not-applied".into());
    }

    spawn_helper_from_scheduled_harness(root, install, server, manager, "recover-manager").await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let transaction_done = InstallTransaction::load(root, install)
            .map_err(|error| format!("transaction-load:{error}"))?
            .is_none();
        let status = load_status(root).map_err(|error| format!("status-load:{error}"))?;
        if transaction_done
            && status.phase == UpdatePhase::Failed
            && status.reason.as_deref() == Some("update-interrupted-and-rolled-back")
        {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "recovery-helper-completion-timeout:transactionDone={transaction_done},phase={:?},reason={:?},failedVersion={:?}",
                status.phase, status.reason, status.last_failed_version
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let restored = wait_for_authenticated_status(&client)
        .await
        .map_err(|error| format!("recovery-health:{error}"))?;
    let status = load_status(root).map_err(|error| format!("status-load:{error}"))?;
    if restored["version"] != env!("CARGO_PKG_VERSION")
        || restored["protocolId"] != PROTOCOL_ID
        || restored["storeFormatId"] != STORE_FORMAT_ID
    {
        return Err("recovery-helper-identity-mismatch".into());
    }
    if fs::read(install.join(marker)).map_err(|_| "restored-marker-read-failed")? != b"old" {
        return Err("recovery-helper-marker-not-restored".into());
    }
    if staged.exists() || backup.exists() {
        return Err("recovery-helper-artifacts-remain".into());
    }
    if status.phase != UpdatePhase::Failed {
        return Err(format!("recovery-helper-phase-mismatch:{:?}", status.phase));
    }
    if status.reason.as_deref() != Some("update-interrupted-and-rolled-back") {
        return Err(format!(
            "recovery-helper-reason-mismatch:{:?}",
            status.reason
        ));
    }
    if status.last_failed_version.as_deref() != Some(target) {
        return Err(format!(
            "recovery-helper-failed-version-mismatch:{:?}",
            status.last_failed_version
        ));
    }
    wait_for_transient_helper_cleanup(root).await?;
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit synthetic binaries and exclusive port 4319"]
async fn interrupted_files_recovery_outlives_the_active_installed_manager() {
    let (source_server, source_manager) = explicit_test_binaries();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("synthetic-data");
    let install = temp.path().join("managed-install");
    let staged = temp
        .path()
        .join(".risunest-sync-update-stage-recovery-live");
    let backup = temp
        .path()
        .join(".risunest-sync-update-backup-recovery-live");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&staged).unwrap();
    let server_name = source_server.file_name().unwrap();
    let manager_name = source_manager.file_name().unwrap();
    let installed_server = install.join(server_name);
    let installed_manager = install.join(manager_name);
    for (source, name) in [
        (&source_server, server_name),
        (&source_manager, manager_name),
    ] {
        fs::copy(source, install.join(name)).unwrap();
        fs::copy(source, staged.join(name)).unwrap();
    }
    fs::write(install.join("recovery-proof.txt"), b"old").unwrap();
    fs::write(staged.join("recovery-proof.txt"), b"new").unwrap();

    let result = live_recovery_from_installed_manager(
        &root,
        &install,
        &staged,
        &backup,
        &installed_server,
        &installed_manager,
    )
    .await;
    let client = Client::new(root.clone()).unwrap();
    let stop = lifecycle::stop(&root, &client).await;
    let unregister = platform::startup(&root, &installed_server, "remove");
    let transient_cleanup = remove_transient_helper_tasks(&root);

    result.unwrap_or_else(|error| {
        panic!(
            "live installed-manager recovery failed at {error}; cleanup stop={stop:?}, unregister={unregister:?}, transient={transient_cleanup:?}"
        )
    });
    stop.unwrap();
    unregister.unwrap();
    transient_cleanup.unwrap();
}

#[cfg(windows)]
async fn live_parent_timeout_recovery(
    root: &Path,
    install: &Path,
    staged: &Path,
    backup: &Path,
    server: &Path,
    files: Vec<PathBuf>,
) -> Result<(), String> {
    platform::startup(root, server, "install")
        .map_err(|error| format!("startup-install:{error}"))?;
    platform::start(root, server).map_err(|error| format!("startup-start:{error}"))?;
    let client = Client::new(root.to_owned()).map_err(|error| format!("client-create:{error}"))?;
    wait_for_authenticated_status(&client)
        .await
        .map_err(|error| format!("initial-health:{error}"))?;
    let target = "99.0.0-synthetic-parent-timeout";
    let transaction = InstallTransaction::new(
        env!("CARGO_PKG_VERSION").into(),
        target.into(),
        TransactionKind::Files,
        install.to_owned(),
        staged.to_owned(),
        backup.to_owned(),
        files,
        true,
    )
    .map_err(|error| format!("transaction-create:{error}"))?;
    transaction
        .save(root)
        .map_err(|error| format!("transaction-save:{error}"))?;
    lifecycle::stop(root, &client)
        .await
        .map_err(|error| format!("lifecycle-stop:{error}"))?;

    let parent_lock = try_lock(root).map_err(|error| format!("parent-lock:{error}"))?;
    let helper_result = run_in_thread(
        root.to_owned(),
        server.to_owned(),
        std::process::id(),
        install.to_owned(),
    );
    let helper_ready_deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if helper_lock_active(root)? {
            break;
        }
        match helper_result.try_recv() {
            Ok(result) => return Err(format!("helper-returned-before-parent-handoff:{result:?}")),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("helper-disconnected-before-parent-handoff".into())
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
        if std::time::Instant::now() >= helper_ready_deadline {
            return Err("helper-lock-acquire-timeout".into());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    drop(parent_lock);
    let error = helper_result
        .recv_timeout(Duration::from_secs(45))
        .map_err(|_| "parent-timeout-helper-result-timeout")?
        .expect_err("the live test process must outlive the helper timeout");
    if error != "update-parent-exit-timeout" {
        return Err(format!("parent-timeout-error:{error}"));
    }
    wait_for_authenticated_status(&client)
        .await
        .map_err(|error| format!("recovery-health:{error}"))?;
    if InstallTransaction::load(root, install)
        .map_err(|error| format!("transaction-load:{error}"))?
        .is_some()
        || staged.exists()
        || backup.exists()
    {
        return Err("parent-timeout-artifacts-remain".into());
    }
    let status = load_status(root).map_err(|error| format!("status-load:{error}"))?;
    if status.phase != UpdatePhase::Failed
        || status.reason.as_deref() != Some("update-parent-exit-timeout")
        || status.last_failed_version.as_deref() != Some(target)
    {
        return Err("parent-timeout-status-mismatch".into());
    }
    Ok(())
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires explicit synthetic binaries, exclusive port 4319, and the real 30 second parent timeout"]
async fn helper_recovers_the_live_server_when_its_parent_does_not_exit() {
    let (source_server, source_manager) = explicit_test_binaries();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("synthetic-data");
    let install = temp.path().join("managed-install");
    let staged = temp
        .path()
        .join(".risunest-sync-update-stage-parent-timeout");
    let backup = temp
        .path()
        .join(".risunest-sync-update-backup-parent-timeout");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&staged).unwrap();
    let server_name = source_server.file_name().unwrap();
    let manager_name = source_manager.file_name().unwrap();
    let installed_server = install.join(server_name);
    for (source, name) in [
        (&source_server, server_name),
        (&source_manager, manager_name),
    ] {
        fs::copy(source, install.join(name)).unwrap();
        fs::copy(source, staged.join(name)).unwrap();
    }
    let files = vec![PathBuf::from(server_name), PathBuf::from(manager_name)];

    let result =
        live_parent_timeout_recovery(&root, &install, &staged, &backup, &installed_server, files)
            .await;
    let client = Client::new(root.clone()).unwrap();
    let stop = lifecycle::stop(&root, &client).await;
    let unregister = platform::startup(&root, &installed_server, "remove");

    result.unwrap_or_else(|error| {
        panic!(
            "live parent-timeout recovery failed at {error}; cleanup stop={stop:?}, unregister={unregister:?}"
        )
    });
    stop.unwrap();
    unregister.unwrap();
}
