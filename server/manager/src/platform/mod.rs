use crate::update::UpdatePolicy;
use crate::Result;
pub mod gui;
use serde::Serialize;
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

pub fn default_data_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    let root = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .map(|p| p.join("RisuNestSync"));
    #[cfg(target_os = "macos")]
    let root = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|p| p.join("Library/Application Support/RisuNestSync"));
    #[cfg(all(unix, not(target_os = "macos")))]
    let root = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
        .map(|p| p.join("risunest-sync"));
    root.filter(|p| p.is_absolute())
        .ok_or("user-data-directory-unavailable".into())
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
    let state = startup(root, executable, "status")?;
    #[cfg(target_os = "macos")]
    let state = if state.registered && !state.action_matches {
        startup(root, executable, "install")?
    } else {
        state
    };
    if state.registered && state.action_matches {
        startup(root, executable, "start")?;
    } else {
        #[cfg(windows)]
        return Err("startup-registration-required".into());
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
        let program = command.get_program().to_owned();
        let arguments = command
            .get_args()
            .map(|value| value.to_owned())
            .collect::<Vec<_>>();
        #[cfg(target_os = "macos")]
        let mut launcher = {
            let mut value = process("launchctl");
            value
                .args([
                    "submit",
                    "-l",
                    &format!(
                        "io.github.rsyumi.{}-apply-{}",
                        instance_name(root),
                        std::process::id()
                    ),
                    "--",
                ])
                .arg(program)
                .args(arguments);
            value
        };
        #[cfg(not(target_os = "macos"))]
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
    #[cfg(windows)]
    windows::spawn_update_helper(root, command)
}

pub fn finish_update_helper(root: &Path, task_name: &str) -> Result<()> {
    #[cfg(windows)]
    return windows::finish_update_helper(root, task_name);
    #[cfg(not(windows))]
    {
        let _ = (root, task_name);
        Err("update-helper-task-invalid".into())
    }
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

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

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
