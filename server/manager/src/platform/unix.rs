use super::*;

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .ok_or("home-unavailable".into())
}
fn checked(command: &mut Command) -> Result<()> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|_| "user-service-unavailable")?;
    if !output.status.success() {
        return Err("user-service-operation-failed".into());
    }
    Ok(())
}
#[cfg(not(target_os = "macos"))]
pub(super) fn startup(root: &Path, executable: &Path, action: &str) -> Result<StartupStatus> {
    let directory = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home()?.join(".config"))
        .join("systemd/user");
    if !directory.is_absolute() {
        return Err("absolute-config-path-required".into());
    }
    let name = format!("{}.service", instance_name(root));
    let path = directory.join(&name);
    match action {
        "install" => {
            std::fs::create_dir_all(&directory).map_err(|_| "user-service-write-failed")?;
            std::fs::write(&path, unit(root, executable)?)
                .map_err(|_| "user-service-write-failed")?;
            checked(process("systemctl").args(["--user", "daemon-reload"]))?;
            checked(process("systemctl").args(["--user", "enable", &name]))?;
        }
        "remove" if path.exists() => {
            checked(process("systemctl").args(["--user", "disable", &name]))?;
            std::fs::remove_file(&path).map_err(|_| "user-service-remove-failed")?;
            checked(process("systemctl").args(["--user", "daemon-reload"]))?;
        }
        "start" => checked(process("systemctl").args(["--user", "start", &name]))?,
        _ => (),
    }
    let enabled = if path.exists() {
        let output = process("systemctl")
            .args(["--user", "is-enabled", &name])
            .output()
            .map_err(|_| "user-service-unavailable")?;
        match String::from_utf8_lossy(&output.stdout).trim() {
            "enabled" => true,
            "disabled" | "masked" | "static" => false,
            _ => return Err("user-service-status-unavailable".into()),
        }
    } else {
        false
    };
    Ok(StartupStatus {
        registered: path.exists(),
        enabled,
        action_matches: true,
    })
}
#[cfg(not(target_os = "macos"))]
fn unit(root: &Path, executable: &Path) -> Result<String> {
    fn arg(path: &Path) -> Result<String> {
        let value = path.to_str().ok_or("invalid-user-service-path")?;
        if value.chars().any(char::is_control) {
            return Err("invalid-user-service-path".into());
        }
        Ok(format!(
            "\"{}\"",
            value
                .replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('%', "%%")
                .replace('$', "$$")
        ))
    }
    Ok(format!("[Unit]\nDescription=RisuNest sync server\nStartLimitIntervalSec=300\nStartLimitBurst=3\n[Service]\nExecStart={} serve --data-dir {}\nRestart=on-failure\nRestartSec=10\n[Install]\nWantedBy=default.target\n",arg(executable)?,arg(root)?))
}

#[cfg(target_os = "macos")]
pub(super) fn startup(root: &Path, executable: &Path, action: &str) -> Result<StartupStatus> {
    let directory = home()?.join("Library/LaunchAgents");
    let name = format!("io.github.rsyumi.{}", instance_name(root));
    let path = directory.join(format!("{name}.plist"));
    let uid = process("id")
        .arg("-u")
        .output()
        .map_err(|_| "user-id-unavailable")?;
    let uid = String::from_utf8(uid.stdout).map_err(|_| "user-id-unavailable")?;
    let domain = format!("gui/{}", uid.trim());
    let service = format!("{domain}/{name}");
    match action {
        "install" => {
            std::fs::create_dir_all(&directory).map_err(|_| "user-service-write-failed")?;
            let body=format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{name}</string><key>ProgramArguments</key><array><string>{}</string><string>serve</string><string>--data-dir</string><string>{}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict><key>ThrottleInterval</key><integer>30</integer></dict></plist>",xml(&executable.to_string_lossy()),xml(&root.to_string_lossy()));
            std::fs::write(&path, body).map_err(|_| "user-service-write-failed")?;
            // Bootstrap reports policy/signature rejection, never silently elevates.
            let loaded = process("launchctl")
                .args(["print", &service])
                .output()
                .map_err(|_| "user-service-unavailable")?
                .status
                .success();
            if !loaded {
                checked(process("launchctl").args(["bootstrap", &domain]).arg(&path))?;
            }
        }
        "remove" if path.exists() => {
            // Removing future login registration must not stop the current daemon.
            // The loaded job leaves the user domain at logout.
            std::fs::remove_file(&path).map_err(|_| "user-service-remove-failed")?;
        }
        "start" => checked(process("launchctl").args(["kickstart", &service]))?,
        _ => (),
    }
    let enabled = path.exists()
        && process("launchctl")
            .args(["print", &service])
            .output()
            .map_err(|_| "user-service-unavailable")?
            .status
            .success();
    Ok(StartupStatus {
        registered: path.exists(),
        enabled,
        action_matches: true,
    })
}
