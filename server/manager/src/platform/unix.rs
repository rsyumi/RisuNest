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
fn systemd_arg(path: &Path) -> Result<String> {
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
    Ok(format!("[Unit]\nDescription=RisuNest sync server\nStartLimitIntervalSec=300\nStartLimitBurst=3\n[Service]\nExecStart={} serve --data-dir {}\nRestart=on-failure\nRestartSec=10\n[Install]\nWantedBy=default.target\n",systemd_arg(executable)?,systemd_arg(root)?))
}

#[cfg(not(target_os = "macos"))]
fn updater_unit(root: &Path, manager: &Path, server: &Path) -> Result<String> {
    Ok(format!(
        "[Unit]\nDescription=Check for a verified RisuNest Sync update\nAfter={}.service\n[Service]\nType=oneshot\nExecStart={} --data-dir {} --server {} update scheduled\n",
        instance_name(root),
        systemd_arg(manager)?,
        systemd_arg(root)?,
        systemd_arg(server)?
    ))
}

#[cfg(not(target_os = "macos"))]
fn updater_timer(update_service: &str, jitter_seconds: u64) -> String {
    format!(
        "[Unit]\nDescription=Schedule verified RisuNest Sync updates\n[Timer]\nOnBootSec={}s\nOnUnitActiveSec=1h\nRandomizedDelaySec=5m\nPersistent=true\nUnit={}\n[Install]\nWantedBy=timers.target\n",
        60 + jitter_seconds,
        update_service
    )
}

#[cfg(not(target_os = "macos"))]
pub(super) fn update_schedule(
    root: &Path,
    manager: &Path,
    server: &Path,
    policy: UpdatePolicy,
    action: &str,
) -> Result<UpdateScheduleStatus> {
    let directory = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or(home()?.join(".config"))
        .join("systemd/user");
    let update_service = format!("{}-update.service", instance_name(root));
    let timer = format!("{}-update.timer", instance_name(root));
    let service_path = directory.join(&update_service);
    let timer_path = directory.join(&timer);
    let should_install = action == "install" && policy != UpdatePolicy::Off;
    if should_install {
        std::fs::create_dir_all(&directory).map_err(|_| "user-service-write-failed")?;
        std::fs::write(&service_path, updater_unit(root, manager, server)?)
            .map_err(|_| "user-service-write-failed")?;
        let jitter = instance_name(root).bytes().fold(0u64, |value, byte| {
            value.wrapping_mul(31).wrapping_add(byte as u64)
        }) % 300;
        std::fs::write(&timer_path, updater_timer(&update_service, jitter))
            .map_err(|_| "user-service-write-failed")?;
        checked(process("systemctl").args(["--user", "daemon-reload"]))?;
        checked(process("systemctl").args(["--user", "enable", "--now", &timer]))?;
    } else if action == "remove" || (action == "install" && policy == UpdatePolicy::Off) {
        if timer_path.exists() {
            checked(process("systemctl").args(["--user", "disable", "--now", &timer]))?;
        }
        if service_path.exists() {
            std::fs::remove_file(&service_path).map_err(|_| "user-service-remove-failed")?;
        }
        if timer_path.exists() {
            std::fs::remove_file(&timer_path).map_err(|_| "user-service-remove-failed")?;
        }
        checked(process("systemctl").args(["--user", "daemon-reload"]))?;
    }
    let enabled = if timer_path.exists() {
        process("systemctl")
            .args(["--user", "is-enabled", &timer])
            .output()
            .map_err(|_| "user-service-unavailable")?
            .status
            .success()
    } else {
        false
    };
    Ok(UpdateScheduleStatus {
        registered: service_path.exists() && timer_path.exists(),
        enabled,
        action_matches: true,
    })
}

#[cfg(all(test, not(target_os = "macos")))]
mod update_tests {
    use super::*;

    #[test]
    fn timer_runs_after_boot_and_every_hour() {
        let body = updater_timer("risunest-sync-test-update.service", 42);
        assert!(body.contains("OnBootSec=102s"));
        assert!(body.contains("OnUnitActiveSec=1h"));
        assert!(body.contains("RandomizedDelaySec=5m"));
        assert!(body.contains("Persistent=true"));
    }

    #[test]
    fn update_service_calls_the_noninteractive_manager_command() {
        let body = updater_unit(
            Path::new("/home/test/.local/share/risunest-sync"),
            Path::new("/home/test/.local/lib/risunest-sync/risunest-sync-manager"),
            Path::new("/home/test/.local/lib/risunest-sync/risunest-sync-server"),
        )
        .unwrap();
        assert!(body.contains("update scheduled"));
        assert!(body.contains("Type=oneshot"));
        assert!(!body.contains("Restart="));
    }

    #[test]
    fn mac_startup_registration_body_tracks_the_current_bundle_path() {
        let root = Path::new("/Users/test/Library/Application Support/RisuNest Sync");
        let old = startup_agent_body(
            "io.github.rsyumi.test",
            root,
            Path::new("/Applications/RisuNest Sync.app/Contents/MacOS/server"),
        );
        let moved = startup_agent_body(
            "io.github.rsyumi.test",
            root,
            Path::new("/Users/test/Applications/RisuNest Sync.app/Contents/MacOS/server"),
        );
        assert_ne!(old, moved);
        assert!(moved.contains("/Users/test/Applications/RisuNest Sync.app"));
    }
}

#[cfg(any(target_os = "macos", test))]
fn startup_agent_body(name: &str, root: &Path, executable: &Path) -> String {
    format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{name}</string><key>ProgramArguments</key><array><string>{}</string><string>serve</string><string>--data-dir</string><string>{}</string></array><key>RunAtLoad</key><true/><key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict><key>ThrottleInterval</key><integer>30</integer></dict></plist>",xml(&executable.to_string_lossy()),xml(&root.to_string_lossy()))
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
    let expected_body = startup_agent_body(&name, root, executable);
    match action {
        "install" => {
            std::fs::create_dir_all(&directory).map_err(|_| "user-service-write-failed")?;
            std::fs::write(&path, &expected_body).map_err(|_| "user-service-write-failed")?;
            // Bootstrap reports policy/signature rejection, never silently elevates.
            let loaded = process("launchctl")
                .args(["print", &service])
                .output()
                .map_err(|_| "user-service-unavailable")?
                .status
                .success();
            if loaded {
                checked(process("launchctl").args(["bootout", &service]))?;
            }
            checked(process("launchctl").args(["bootstrap", &domain]).arg(&path))?;
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
    let action_matches =
        path.exists() && std::fs::read_to_string(&path).is_ok_and(|body| body == expected_body);
    Ok(StartupStatus {
        registered: path.exists(),
        enabled,
        action_matches,
    })
}

#[cfg(target_os = "macos")]
pub(super) fn update_schedule(
    root: &Path,
    manager: &Path,
    server: &Path,
    policy: UpdatePolicy,
    action: &str,
) -> Result<UpdateScheduleStatus> {
    let directory = home()?.join("Library/LaunchAgents");
    let name = format!("io.github.rsyumi.{}-update", instance_name(root));
    let path = directory.join(format!("{name}.plist"));
    let uid = process("id")
        .arg("-u")
        .output()
        .map_err(|_| "user-id-unavailable")?;
    let uid = String::from_utf8(uid.stdout).map_err(|_| "user-id-unavailable")?;
    let domain = format!("gui/{}", uid.trim());
    let service = format!("{domain}/{name}");
    let should_install = action == "install" && policy != UpdatePolicy::Off;
    let expected_body = format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?><plist version=\"1.0\"><dict><key>Label</key><string>{name}</string><key>ProgramArguments</key><array><string>{}</string><string>--data-dir</string><string>{}</string><string>--server</string><string>{}</string><string>update</string><string>scheduled</string></array><key>RunAtLoad</key><true/><key>StartInterval</key><integer>3600</integer><key>ProcessType</key><string>Background</string><key>AbandonProcessGroup</key><true/></dict></plist>", xml(&manager.to_string_lossy()), xml(&root.to_string_lossy()), xml(&server.to_string_lossy()));
    if should_install {
        std::fs::create_dir_all(&directory).map_err(|_| "user-service-write-failed")?;
        std::fs::write(&path, &expected_body).map_err(|_| "user-service-write-failed")?;
        let loaded = process("launchctl")
            .args(["print", &service])
            .output()
            .map_err(|_| "user-service-unavailable")?
            .status
            .success();
        if loaded {
            checked(process("launchctl").args(["bootout", &service]))?;
        }
        checked(process("launchctl").args(["bootstrap", &domain]).arg(&path))?;
    } else if (action == "remove" || (action == "install" && policy == UpdatePolicy::Off))
        && path.exists()
    {
        let _ = process("launchctl").args(["bootout", &service]).output();
        std::fs::remove_file(&path).map_err(|_| "user-service-remove-failed")?;
    }
    let enabled = path.exists()
        && process("launchctl")
            .args(["print", &service])
            .output()
            .map_err(|_| "user-service-unavailable")?
            .status
            .success();
    Ok(UpdateScheduleStatus {
        registered: path.exists(),
        enabled,
        action_matches: path.exists()
            && std::fs::read_to_string(&path).is_ok_and(|body| body == expected_body),
    })
}
