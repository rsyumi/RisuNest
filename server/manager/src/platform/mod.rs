use crate::update::UpdatePolicy;
use crate::Result;
pub mod gui;
use serde::Serialize;
#[cfg(target_os = "macos")]
use std::{
    fs::{self, File},
    io::Write,
};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
};
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupStatus {
    pub registered: bool,
    pub enabled: bool,
    pub action_matches: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateScheduleStatus {
    pub registered: bool,
    pub enabled: bool,
    pub action_matches: bool,
}

#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn user_service_directory(xdg: Option<PathBuf>, home: impl FnOnce() -> Result<PathBuf>) -> Result<PathBuf> {
    let config = match xdg.filter(|path| path.is_absolute()) {
        Some(path) => path,
        None => {
            let home = home()?;
            if !home.is_absolute() {
                return Err("absolute-config-path-required".into());
            }
            home.join(".config")
        }
    };
    Ok(config.join("systemd/user"))
}

pub fn default_data_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let root = windows_data_root(
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        "RisuNestSyncData",
    );
    #[cfg(target_os = "macos")]
    let root = macos_data_root(std::env::var_os("HOME").map(PathBuf::from));
    #[cfg(all(unix, not(target_os = "macos")))]
    let root = linux_data_root(
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        "risunest-sync",
    );
    root
        .ok_or("user-data-directory-unavailable".into())
}

pub fn webview_data_dir() -> Result<Option<PathBuf>> {
    #[cfg(target_os = "macos")]
    return Ok(None);
    #[cfg(windows)]
    let root = windows_data_root(
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
        "RisuNestSyncWebViewData",
    );
    #[cfg(all(unix, not(target_os = "macos")))]
    let root = linux_data_root(
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
        "risunest-sync-webview",
    );
    #[cfg(not(target_os = "macos"))]
    {
        root.map(Some)
            .ok_or("user-data-directory-unavailable".into())
    }
}

#[cfg(any(test, windows))]
fn windows_data_root(base: Option<PathBuf>, leaf: &str) -> Option<PathBuf> {
    base.filter(|path| path.is_absolute())
        .map(|path| path.join(leaf))
}

#[cfg(any(test, target_os = "macos"))]
fn macos_data_root(home: Option<PathBuf>) -> Option<PathBuf> {
    home.filter(|path| path.is_absolute()).map(|path| {
        path.join("Library/Application Support/io.github.rsyumi.risunest.sync-manager")
    })
}

#[cfg(any(test, all(unix, not(target_os = "macos"))))]
fn linux_data_root(xdg: Option<PathBuf>, home: Option<PathBuf>, leaf: &str) -> Option<PathBuf> {
    xdg.filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(".local/share"))
        })
        .map(|path| path.join(leaf))
}

pub fn server_executable() -> Result<PathBuf> {
    let current = std::env::current_exe().map_err(|_| "executable-unavailable")?;
    let name = if cfg!(windows) {
        "risunest-sync-server.exe"
    } else {
        "risunest-sync-server"
    };
    Ok(current.parent().ok_or("executable-unavailable")?.join(name))
}

pub fn manager_executable() -> Result<PathBuf> {
    let current = std::env::current_exe().map_err(|_| "executable-unavailable")?;
    let name = if cfg!(windows) {
        "risunest-sync-manager.exe"
    } else {
        "risunest-sync-manager"
    };
    Ok(current.parent().ok_or("executable-unavailable")?.join(name))
}

pub fn process(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let mut command = command;
        command.creation_flags(0x08000000);
        command
    }
    #[cfg(not(windows))]
    command
}

pub fn initialize(root: &Path, executable: &Path) -> Result<()> {
    if !root.is_absolute() || !executable.is_absolute() || !executable.is_file() {
        return Err("server-executable-or-data-path-invalid".into());
    }
    if !root.join("metadata.sqlite").exists() {
        let output = process(executable)
            .args(["init", "--data-dir"])
            .arg(root)
            .stdin(Stdio::null())
            .output()
            .map_err(|_| "server-init-failed")?;
        if !output.status.success() {
            return Err("server-init-failed".into());
        }
    }
    Ok(())
}

pub fn start(root: &Path, executable: &Path) -> Result<()> {
    initialize(root, executable)?;
    let _ = std::fs::remove_file(root.join("startup-error.txt"));
    let state = startup(root, executable, "status")?;
    #[cfg(target_os = "macos")]
    let state = if state.registered && !state.action_matches {
        startup(root, executable, "install")?
    } else {
        state
    };
    if state.registered && state.enabled && state.action_matches {
        startup(root, executable, "start")?;
    } else {
        #[cfg(windows)]
        windows::startup(root, executable, "manual")?;
        #[cfg(not(windows))]
        {
            let mut command = process(executable);
            command
                .args(["serve", "--data-dir"])
                .arg(root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            spawn_background(&mut command)?;
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn spawn_background(command: &mut Command) -> Result<u32> {
    let mut child = command.spawn().map_err(|_| "server-start-failed")?;
    let pid = child.id();
    std::thread::Builder::new()
        .name("risunest-server-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .map_err(|_| "server-reaper-unavailable")?;
    Ok(pid)
}

pub fn startup(root: &Path, executable: &Path, action: &str) -> Result<StartupStatus> {
    if !["status", "install", "remove", "start"].contains(&action) {
        return Err("invalid-startup-action".into());
    }
    if !root.is_absolute() || !executable.is_absolute() {
        return Err("absolute-path-required".into());
    }
    if action == "install" {
        initialize(root, executable)?;
    }
    #[cfg(windows)]
    {
        windows::startup(root, executable, action)
    }
    #[cfg(unix)]
    {
        unix::startup(root, executable, action)
    }
}

pub fn spawn_update_helper(root: &Path, command: &mut Command) -> Result<u32> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        #[cfg(target_os = "macos")]
        return spawn_macos_update_helper(root, command);
        #[cfg(not(target_os = "macos"))]
        {
            let program = command.get_program().to_owned();
            let arguments = command
                .get_args()
                .map(|value| value.to_owned())
                .collect::<Vec<_>>();
            let mut launcher = {
                let mut value = process("systemd-run");
                value
                    .args(["--user", "--collect", "--quiet", "--unit"])
                    .arg(format!("{}-update-apply", instance_name(root)))
                    .arg(program)
                    .args(arguments);
                value
            };
            let output = launcher
                .stdin(Stdio::null())
                .output()
                .map_err(|_| "update-helper-start-failed".to_owned())?;
            if !output.status.success() {
                return Err("update-helper-start-failed".into());
            }
            return Ok(0);
        }
    }
    #[cfg(windows)]
    windows::spawn_update_helper(root, command)
}

pub fn finish_update_helper(root: &Path, task_name: &str) -> Result<()> {
    #[cfg(windows)]
    return windows::finish_update_helper(root, task_name);
    #[cfg(target_os = "macos")]
    return finish_macos_update_helper(root, task_name);
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = (root, task_name);
        Err("update-helper-task-invalid".into())
    }
}

pub fn cleanup_update_helpers(root: &Path) -> Result<()> {
    #[cfg(windows)]
    return windows::cleanup_update_helpers(root);
    #[cfg(target_os = "macos")]
    return cleanup_macos_update_helpers(root);
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _ = root;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn launchd_user_domain() -> Result<String> {
    let output = process("id")
        .arg("-u")
        .output()
        .map_err(|_| "user-id-unavailable".to_owned())?;
    if !output.status.success() {
        return Err("user-id-unavailable".into());
    }
    let uid = String::from_utf8(output.stdout).map_err(|_| "user-id-unavailable".to_owned())?;
    Ok(format!("gui/{}", uid.trim()))
}

#[cfg(target_os = "macos")]
fn macos_helper_prefix(root: &Path) -> String {
    format!("io.github.rsyumi.{}-update-helper-", instance_name(root))
}

#[cfg(target_os = "macos")]
fn macos_helper_plist(root: &Path, task_name: &str) -> PathBuf {
    root.join("manager-update/helper")
        .join(format!("{task_name}.plist"))
}

#[cfg(target_os = "macos")]
fn valid_macos_helper_task(root: &Path, task_name: &str) -> bool {
    let prefix = macos_helper_prefix(root);
    task_name.starts_with(&prefix)
        && task_name.len() == prefix.len() + 64
        && task_name[prefix.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(target_os = "macos")]
fn spawn_macos_update_helper(root: &Path, command: &mut Command) -> Result<u32> {
    let task_name = format!(
        "{}{}",
        macos_helper_prefix(root),
        risunest_sync_server::management::discovery::request_id()
            .map_err(|_| "update-helper-start-failed".to_owned())?
    );
    command.args(["--scheduled-task", &task_name]);
    let program = command
        .get_program()
        .to_str()
        .ok_or("update-helper-start-failed")?;
    let arguments = command
        .get_args()
        .map(|value| {
            value
                .to_str()
                .ok_or_else(|| "update-helper-start-failed".to_owned())
        })
        .collect::<Result<Vec<_>>>()?;
    let mut body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{}</string><key>ProgramArguments</key><array><string>{}</string>",
        xml(&task_name),
        xml(program)
    );
    for argument in arguments {
        body.push_str("<string>");
        body.push_str(&xml(argument));
        body.push_str("</string>");
    }
    body.push_str("</array><key>RunAtLoad</key><true/><key>KeepAlive</key><false/><key>LaunchOnlyOnce</key><true/><key>ProcessType</key><string>Background</string><key>AbandonProcessGroup</key><true/></dict></plist>");
    let path = macos_helper_plist(root, &task_name);
    let directory = path.parent().ok_or("update-helper-start-failed")?;
    fs::create_dir_all(directory).map_err(|_| "update-helper-start-failed".to_owned())?;
    let domain = launchd_user_domain()?;
    let mut file = tempfile::NamedTempFile::new_in(directory)
        .map_err(|_| "update-helper-start-failed".to_owned())?;
    file.write_all(body.as_bytes())
        .and_then(|()| file.as_file().sync_all())
        .map_err(|_| "update-helper-start-failed".to_owned())?;
    file.persist(&path)
        .map_err(|_| "update-helper-start-failed".to_owned())?;
    if File::open(directory)
        .and_then(|value| value.sync_all())
        .is_err()
    {
        let _ = fs::remove_file(&path);
        let _ = File::open(directory).and_then(|value| value.sync_all());
        return Err("update-helper-start-failed".into());
    }
    let output = process("launchctl")
        .args(["bootstrap", &domain])
        .arg(&path)
        .stdin(Stdio::null())
        .output();
    if output.is_err() || !output.is_ok_and(|value| value.status.success()) {
        let _ = fs::remove_file(&path);
        let _ = File::open(directory).and_then(|value| value.sync_all());
        return Err("update-helper-start-failed".into());
    }
    Ok(0)
}

#[cfg(target_os = "macos")]
fn finish_macos_update_helper(root: &Path, task_name: &str) -> Result<()> {
    if !valid_macos_helper_task(root, task_name) {
        return Err("update-helper-task-invalid".into());
    }
    let path = macos_helper_plist(root, task_name);
    let directory = path.parent().ok_or("update-helper-task-invalid")?;
    if path.exists() {
        fs::remove_file(&path).map_err(|_| "update-helper-task-cleanup-failed".to_owned())?;
        File::open(directory)
            .and_then(|value| value.sync_all())
            .map_err(|_| "update-helper-task-cleanup-failed".to_owned())?;
    }
    let service = format!("{}/{}", launchd_user_domain()?, task_name);
    process("launchctl")
        .args(["bootout", &service])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "update-helper-task-cleanup-failed".to_owned())?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn cleanup_macos_update_helpers(root: &Path) -> Result<()> {
    let directory = root.join("manager-update/helper");
    if !directory.exists() {
        return Ok(());
    }
    let mut tasks = Vec::new();
    for entry in
        fs::read_dir(&directory).map_err(|_| "update-helper-task-cleanup-failed".to_owned())?
    {
        let entry = entry.map_err(|_| "update-helper-task-cleanup-failed".to_owned())?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("plist") {
            continue;
        }
        let task = path
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or("update-helper-task-cleanup-failed")?;
        if !valid_macos_helper_task(root, task) {
            return Err("update-helper-task-cleanup-failed".into());
        }
        tasks.push(task.to_owned());
    }
    let domain = launchd_user_domain()?;
    for task in tasks {
        let path = macos_helper_plist(root, &task);
        let service = format!("{domain}/{task}");
        let present = process("launchctl")
            .args(["print", &service])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| "update-helper-task-cleanup-failed".to_owned())?
            .success();
        if present {
            let status = process("launchctl")
                .args(["bootout", &service])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .map_err(|_| "update-helper-task-cleanup-failed".to_owned())?;
            if !status.success() {
                return Err("update-helper-task-cleanup-failed".into());
            }
        }
        fs::remove_file(&path).map_err(|_| "update-helper-task-cleanup-failed".to_owned())?;
        File::open(&directory)
            .and_then(|value| value.sync_all())
            .map_err(|_| "update-helper-task-cleanup-failed".to_owned())?;
    }
    Ok(())
}

pub fn wait_for_parent_exit(process_id: u32, timeout: std::time::Duration) -> Result<()> {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::{
            Foundation::CloseHandle,
            System::Threading::{OpenProcess, WaitForSingleObject},
        };
        let handle = OpenProcess(0x00100000, 0, process_id);
        if handle.is_null() {
            return Ok(());
        }
        let milliseconds = timeout.as_millis().min(u32::MAX as u128) as u32;
        let result = WaitForSingleObject(handle, milliseconds);
        CloseHandle(handle);
        match result {
            0 => Ok(()),
            0x00000102 => Err("update-parent-exit-timeout".into()),
            _ => Err("update-parent-wait-failed".into()),
        }
    }
    #[cfg(unix)]
    {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if unsafe { libc::kill(process_id as i32, 0) } != 0 {
                let error = std::io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::ESRCH) => return Ok(()),
                    Some(libc::EPERM) => {}
                    _ => return Err("update-parent-wait-failed".into()),
                }
            }
            if std::time::Instant::now() >= deadline {
                return Err("update-parent-exit-timeout".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

pub fn update_schedule(
    root: &Path,
    manager: &Path,
    server: &Path,
    policy: UpdatePolicy,
    action: &str,
) -> Result<UpdateScheduleStatus> {
    if !["status", "install", "remove"].contains(&action) {
        return Err("invalid-update-schedule-action".into());
    }
    if !root.is_absolute() || !manager.is_absolute() || !server.is_absolute() {
        return Err("absolute-path-required".into());
    }
    #[cfg(windows)]
    {
        windows::update_schedule(root, manager, server, policy, action)
    }
    #[cfg(unix)]
    {
        unix::update_schedule(root, manager, server, policy, action)
    }
}

pub fn instance_name(root: &Path) -> String {
    let hash = root
        .as_os_str()
        .to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x100000001b3)
        });
    format!("risunest-sync-{hash:016x}")
}
pub fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_xdg_config_does_not_require_home() {
        let root = tempfile::tempdir().unwrap();
        let xdg = root.path().join("config");
        let resolved = user_service_directory(Some(xdg.clone()), || panic!("HOME must stay lazy")).unwrap();
        assert_eq!(resolved, xdg.join("systemd/user"));
    }

    #[test]
    fn absent_empty_or_relative_xdg_uses_an_absolute_home() {
        let root = tempfile::tempdir().unwrap();
        for xdg in [None, Some(PathBuf::new()), Some(PathBuf::from("relative/config"))] {
            let resolved = user_service_directory(xdg.clone(), || Ok(root.path().to_owned())).unwrap();
            assert_eq!(resolved, root.path().join(".config/systemd/user"));
            assert!(user_service_directory(xdg.clone(), || Err("home-unavailable".into())).is_err());
            assert!(user_service_directory(xdg, || Ok(PathBuf::from("relative/home"))).is_err());
        }
    }

    #[test]
    fn platform_data_leaf_names_follow_native_conventions() {
        let windows_base = tempfile::tempdir().unwrap();
        let linux_home = tempfile::tempdir().unwrap();
        let macos_home = tempfile::tempdir().unwrap();
        assert_eq!(
            windows_data_root(Some(windows_base.path().to_owned()), "RisuNestSyncData"),
            Some(windows_base.path().join("RisuNestSyncData"))
        );
        assert_eq!(
            windows_data_root(
                Some(windows_base.path().to_owned()),
                "RisuNestSyncWebViewData"
            ),
            Some(windows_base.path().join("RisuNestSyncWebViewData"))
        );
        assert_eq!(
            linux_data_root(None, Some(linux_home.path().to_owned()), "risunest-sync"),
            Some(linux_home.path().join(".local/share/risunest-sync"))
        );
        assert_eq!(
            linux_data_root(
                None,
                Some(linux_home.path().to_owned()),
                "risunest-sync-webview"
            ),
            Some(linux_home.path().join(".local/share/risunest-sync-webview"))
        );
        assert_eq!(
            macos_data_root(Some(macos_home.path().to_owned())),
            Some(macos_home.path().join(
                "Library/Application Support/io.github.rsyumi.risunest.sync-manager"
            ))
        );
        assert!(
            windows_data_root(Some(PathBuf::from("relative")), "RisuNestSyncData").is_none()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn background_children_are_reaped_after_exit() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 0"]);
        let pid = spawn_background(&mut command).unwrap();
        for _ in 0..50 {
            let status = Command::new("ps")
                .args(["-p", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap();
            if !status.success() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("background child was not reaped");
    }
}
