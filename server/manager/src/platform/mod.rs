use crate::update::UpdatePolicy;
use crate::Result;
pub mod gui;
pub mod paths;
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

#[cfg(unix)]
fn user_service_directory() -> Result<PathBuf> {
    paths::current()?
        .user_services
        .ok_or_else(|| "absolute-config-path-required".into())
}

pub fn default_data_dir() -> Result<PathBuf> {
    Ok(paths::current()?.data)
}

pub fn resolve_manager_root(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err("absolute-data-dir-required".into());
    }
    if path
        .components()
        .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err("unsafe-storage-path".into());
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            #[cfg(windows)]
            let linked = {
                use std::os::windows::fs::MetadataExt;
                metadata.file_attributes() & 0x400 != 0
            };
            #[cfg(not(windows))]
            let linked = metadata.file_type().is_symlink();
            if linked {
                return Err("unsafe-storage-path".into());
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
        Err(_) => return Err("storage-io".into()),
    }
    let mut ancestor = path.parent().ok_or("invalid-data-dir")?;
    let mut missing = vec![path.file_name().ok_or("invalid-data-dir")?.to_owned()];
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for name in missing.iter().rev() {
                    resolved.push(name);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match std::fs::symlink_metadata(ancestor) {
                    Ok(_) => return Err("unsafe-storage-path".into()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(_) => return Err("storage-io".into()),
                }
                missing.push(ancestor.file_name().ok_or("invalid-data-dir")?.to_owned());
                ancestor = ancestor.parent().ok_or("invalid-data-dir")?;
            }
            Err(_) => return Err("storage-io".into()),
        }
    }
}

pub fn webview_data_dir() -> Result<Option<PathBuf>> {
    Ok(paths::current()?.webview)
}

/// The directories Tauri would resolve from the GUI identifier. Nothing writes
/// to them, so they are swept with the GUI profile rather than owned.
pub fn identifier_directories() -> Result<Vec<PathBuf>> {
    Ok(paths::current()?.tauri_derived)
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

/// The manager build that Task Scheduler and detached update helpers run.
/// Windows uses the windowless sibling so these runs open no console window.
pub fn background_manager_executable(manager: &Path) -> PathBuf {
    if cfg!(windows) {
        manager.with_file_name("risunest-sync-manager-background.exe")
    } else {
        manager.to_owned()
    }
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

pub fn startup_error_code(value: &str) -> Option<String> {
    let code = value.lines().next()?.trim();
    (!code.is_empty()
        && code.len() <= 64
        && code
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'))
    .then(|| code.to_owned())
}

pub fn startup_error(root: &Path) -> Option<String> {
    use std::io::Read;
    let mut value = String::new();
    std::fs::File::open(root.join("startup-error.txt"))
        .ok()?
        .take(65)
        .read_to_string(&mut value)
        .ok()?;
    startup_error_code(&value)
}

pub fn initialize(root: &Path, executable: &Path) -> Result<()> {
    let resolved = resolve_manager_root(root)?;
    let root = resolved.as_path();
    if !root.is_absolute() || !executable.is_absolute() || !executable.is_file() {
        return Err("server-executable-or-data-path-invalid".into());
    }
    if !root.join("metadata.sqlite").exists() {
        let output = process(executable)
            .args(["init", "--data-dir"])
            .arg(root)
            .stdin(Stdio::null())
            .output()
            .map_err(|error| {
                format!(
                    "server-init-launch-failed: {} ({error})",
                    executable.display()
                )
            })?;
        if !output.status.success() {
            return Err(startup_error_code(&String::from_utf8_lossy(&output.stderr))
                .unwrap_or_else(|| format!("server-init-failed: exit={}", output.status)));
        }
    }
    crate::removal::register(root, executable)?;
    Ok(())
}

pub fn start(root: &Path, executable: &Path) -> Result<()> {
    initialize(root, executable)?;
    let _ = std::fs::remove_file(root.join("startup-error.txt"));
    let state = startup(root, executable, "status")?;
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

pub(crate) fn reconcile_relocated_registration(root: &Path, server: &Path) -> Result<()> {
    let state = startup(root, server, "status")?;
    if state.registered && !state.action_matches {
        if !state.enabled {
            return Err("update-startup-state-not-verifiable".into());
        }
        #[cfg(windows)]
        windows::startup(root, server, "install")?;
        #[cfg(unix)]
        unix::startup(root, server, "install")?;
    }
    #[cfg(any(windows, target_os = "macos"))]
    gui::reconcile(
        root,
        &server.with_file_name(if cfg!(windows) {
            "risunest-sync-gui.exe"
        } else {
            "risunest-sync-gui"
        }),
    )?;
    let manager = server.with_file_name(if cfg!(windows) {
        "risunest-sync-manager.exe"
    } else {
        "risunest-sync-manager"
    });
    crate::update::reconcile_schedule_while_locked(root, &manager, server)
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

pub fn spawn_installer_guard(root: &Path, command: &mut Command) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        windows::spawn_update_helper(root, command)?;
        Ok(())
    }
    #[cfg(not(windows))]
    {
        command
            .spawn()
            .map(|_| ())
            .map_err(|_| "installer-guard-unavailable".into())
    }
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

#[cfg(any(target_os = "macos", test))]
fn macos_helper_prefix(root: &Path) -> String {
    format!("io.github.rsyumi.{}-update-helper-", instance_name(root))
}

#[cfg(target_os = "macos")]
fn macos_helper_plist(root: &Path, task_name: &str) -> PathBuf {
    root.join("manager-update/helper")
        .join(format!("{task_name}.plist"))
}

#[cfg(any(target_os = "macos", test))]
fn valid_macos_helper_task(root: &Path, task_name: &str) -> bool {
    let prefix = macos_helper_prefix(root);
    task_name.starts_with(&prefix)
        && risunest_sync_server::identity::is_uuid_v4(&task_name[prefix.len()..])
}

#[test]
fn macos_helper_names_require_own_root_and_canonical_uuid() {
    let root = Path::new("synthetic-instance");
    let prefix = macos_helper_prefix(root);
    let id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    assert!(valid_macos_helper_task(root, &format!("{prefix}{id}")));
    assert!(!valid_macos_helper_task(
        Path::new("other-instance"),
        &format!("{prefix}{id}")
    ));
    for invalid in [
        id.to_uppercase(),
        id.replace("-4aaa-", "-7aaa-"),
        id.replace("-8aaa-", "-0aaa-"),
        format!("{id}\n"),
        "a".repeat(64),
    ] {
        assert!(!valid_macos_helper_task(
            root,
            &format!("{prefix}{invalid}")
        ));
    }
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
    remove_macos_helper_plist(&macos_helper_plist(root, task_name))?;
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
        remove_macos_helper(
            &format!("{domain}/{task}"),
            &macos_helper_plist(root, &task),
        )?;
    }
    Ok(())
}

// A helper removes its own plist and boots itself out as it exits, so a helper
// that disappears while another process removes it has already been removed.
#[cfg(target_os = "macos")]
fn remove_macos_helper(service: &str, path: &Path) -> Result<()> {
    if launchd_service_loaded(service)? {
        let status = process("launchctl")
            .args(["bootout", service])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| "update-helper-task-cleanup-failed".to_owned())?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !status.success() && launchd_service_loaded(service)? {
            if std::time::Instant::now() >= deadline {
                return Err("update-helper-task-cleanup-failed".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    remove_macos_helper_plist(path)
}

#[cfg(target_os = "macos")]
fn launchd_service_loaded(service: &str) -> Result<bool> {
    Ok(process("launchctl")
        .args(["print", service])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|_| "update-helper-task-cleanup-failed".to_owned())?
        .success())
}

#[cfg(target_os = "macos")]
fn remove_macos_helper_plist(path: &Path) -> Result<()> {
    let gone = |error: &std::io::Error| error.kind() == std::io::ErrorKind::NotFound;
    match fs::remove_file(path) {
        Err(error) if gone(&error) => return Ok(()),
        result => result.map_err(|_| "update-helper-task-cleanup-failed".to_owned())?,
    }
    let directory = path.parent().ok_or("update-helper-task-cleanup-failed")?;
    match File::open(directory).and_then(|value| value.sync_all()) {
        Err(error) if !gone(&error) => Err("update-helper-task-cleanup-failed".into()),
        _ => Ok(()),
    }
}

#[cfg(windows)]
pub struct InstallerOwner(windows_sys::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for InstallerOwner {}
#[cfg(windows)]
unsafe impl Sync for InstallerOwner {}
#[cfg(windows)]
impl InstallerOwner {
    pub fn open(pid: u32) -> Result<Self> {
        if pid == 0 {
            return Err("invalid-installer-owner".into());
        }
        let handle = unsafe {
            windows_sys::Win32::System::Threading::OpenProcess(0x00100000 | 0x1000, 0, pid)
        };
        if handle.is_null() {
            return Err("installer-owner-unavailable".into());
        }
        Ok(Self(handle))
    }
    pub fn is_alive(&self) -> Result<bool> {
        match unsafe { windows_sys::Win32::System::Threading::WaitForSingleObject(self.0, 0) } {
            0 => Ok(false),
            0x00000102 => Ok(true),
            _ => Err("installer-owner-unavailable".into()),
        }
    }
    pub fn started(&self) -> Result<u64> {
        use windows_sys::Win32::{Foundation::FILETIME, System::Threading::GetProcessTimes};
        let mut created = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let mut exited = created;
        let mut kernel = created;
        let mut user = created;
        if unsafe { GetProcessTimes(self.0, &mut created, &mut exited, &mut kernel, &mut user) }
            == 0
        {
            return Err("installer-owner-unavailable".into());
        }
        Ok((u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }
}
#[cfg(windows)]
impl Drop for InstallerOwner {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
pub fn sweep_stale_update_helpers(root: &Path) -> Result<()> {
    windows::sweep_stale_update_helpers(root)
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
    if !["status", "install", "install-recovery", "remove"].contains(&action) {
        return Err("invalid-update-schedule-action".into());
    }
    if !root.is_absolute() || !manager.is_absolute() || !server.is_absolute() {
        return Err("absolute-path-required".into());
    }
    let policy = if action == "install-recovery" {
        UpdatePolicy::Automatic
    } else {
        policy
    };
    #[cfg(windows)]
    {
        let action = if action == "install-recovery" {
            "install"
        } else {
            action
        };
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
    fn root_resolution_preserves_missing_parents_without_creating_them() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("missing/nested/data");
        assert_eq!(
            resolve_manager_root(&root).unwrap(),
            temp.path()
                .canonicalize()
                .unwrap()
                .join("missing/nested/data")
        );
        assert!(!temp.path().join("missing").exists());
        assert_eq!(
            resolve_manager_root(&temp.path().join("missing/../data")).unwrap_err(),
            "unsafe-storage-path"
        );
    }

    #[cfg(unix)]
    #[test]
    fn root_resolution_accepts_linked_ancestors_but_refuses_a_linked_live_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let actual = temp.path().join("actual");
        std::fs::create_dir(&actual).unwrap();
        let alias = temp.path().join("alias");
        std::os::unix::fs::symlink(&actual, &alias).unwrap();
        assert_eq!(
            resolve_manager_root(&alias.join("new/data")).unwrap(),
            actual.canonicalize().unwrap().join("new/data")
        );
        assert_eq!(
            resolve_manager_root(&alias).unwrap_err(),
            "unsafe-storage-path"
        );
        assert!(!actual.join("new").exists());
    }

    #[test]
    fn startup_failures_are_bounded_codes_and_not_arbitrary_stderr() {
        assert_eq!(
            startup_error_code("incompatible-store\nprivate diagnostic"),
            Some("incompatible-store".into())
        );
        for invalid in ["", "Error: private path", "path/to/store", &"a".repeat(65)] {
            assert_eq!(startup_error_code(invalid), None);
        }
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("startup-error.txt"), "data-dir-busy").unwrap();
        assert_eq!(startup_error(root.path()), Some("data-dir-busy".into()));
        std::fs::write(root.path().join("startup-error.txt"), "x".repeat(100)).unwrap();
        assert_eq!(startup_error(root.path()), None);
    }

    #[cfg(windows)]
    #[test]
    fn installer_owner_handle_tracks_the_process_lifetime() {
        let mut child = process("cmd.exe")
            .args(["/C", "exit", "0"])
            .spawn()
            .unwrap();
        let owner = InstallerOwner::open(child.id()).unwrap();
        child.wait().unwrap();
        assert!(!owner.is_alive().unwrap());
        assert!(InstallerOwner::open(std::process::id())
            .unwrap()
            .is_alive()
            .unwrap());
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

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_helpers_that_removed_themselves_count_as_removed() {
        let root = tempfile::tempdir().unwrap();
        let task = format!(
            "{}{}",
            macos_helper_prefix(root.path()),
            risunest_sync_server::management::discovery::request_id().unwrap()
        );
        let path = macos_helper_plist(root.path(), &task);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let service = format!("{}/{task}", launchd_user_domain().unwrap());
        assert_eq!(remove_macos_helper(&service, &path), Ok(()));
    }
}

pub fn remove_startup(root: &Path, executable: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        startup(root, executable, "remove").map(|_| ())
    }
    #[cfg(unix)]
    {
        unix::remove_startup(root, executable)
    }
}
