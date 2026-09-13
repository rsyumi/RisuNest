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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupStatus {
    pub registered: bool,
    pub enabled: bool,
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
    if startup(root, executable, "status")?.registered {
        startup(root, executable, "start")?;
    } else {
        #[cfg(windows)]
        return Err("startup-registration-required".into());
        #[cfg(not(windows))]
        {
            process(executable)
                .args(["serve", "--data-dir"])
                .arg(root)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|_| "server-start-failed")?;
        }
    }
    Ok(())
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
