use risunest_sync_manager::{client::Client, platform, tui, update, Result};
use std::path::PathBuf;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("{} ({})", tui::error_message(&error), error);
        std::process::exit(1);
    }
}
async fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut root = platform::default_data_dir()?;
    let mut executable = platform::server_executable()?;
    let mut command = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => root = PathBuf::from(args.next().ok_or("data-dir-required")?),
            "--server" => executable = PathBuf::from(args.next().ok_or("server-path-required")?),
            "--help" | "-h" => {
                println!("risunest-sync-manager [--data-dir PATH] [--server EXECUTABLE] [status|start|stop|autostart install|autostart remove|update status|update policy automatic|notify|off|update check]\n명령을 생략하면 관리 메뉴를 엽니다.");
                return Ok(());
            }
            _ => command.push(arg),
        }
    }
    if !root.is_absolute() {
        return Err("absolute-data-dir-required".into());
    }
    let client = Client::new(root.clone())?;
    let manager = platform::manager_executable()?;
    match command
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [] => tui::run(&root, &executable).await,
        ["status"] => {
            println!("{}", client.status().await?);
            Ok(())
        }
        ["start"] => start_server(&root, &executable, &client).await,
        ["start", "lock-held"] => start_server(&root, &executable, &client).await,
        ["stop"] => {
            risunest_sync_manager::lifecycle::stop(&root, &client).await?;
            println!("서버 중지됨");
            Ok(())
        }
        ["prepare-update"] => {
            let _lock = update::try_lock(&root)?;
            if root.join("manager-update/transaction.json").exists() {
                return Err("update-recovery-required".into());
            }
            risunest_sync_manager::lifecycle::stop(&root, &client).await
        }
        ["prepare-update", "lock-held"] => {
            if root.join("manager-update/transaction.json").exists() {
                return Err("update-recovery-required".into());
            }
            risunest_sync_manager::lifecycle::stop(&root, &client).await
        }
        ["installer", "prepare"] => {
            println!("{}", update::begin_installer_guard(&root, &executable)?);
            Ok(())
        }
        ["installer", "swap", "lock-held", staged, was_running] => {
            let was_running = match *was_running {
                "true" => true,
                "false" => false,
                _ => return Err("installer-running-intent-invalid".into()),
            };
            update::installer_swap_while_locked(
                &root,
                &executable,
                &PathBuf::from(staged),
                was_running,
            )
        }
        ["installer", "sync-stage", "lock-held", staged] => {
            update::installer_sync_stage_while_locked(&PathBuf::from(staged))
        }
        ["installer", "commit-swap", "lock-held"] => {
            update::installer_commit_swap_while_locked(&root, &executable)
        }
        ["installer", "rollback-swap", "lock-held", was_running] => {
            let was_running = match *was_running {
                "true" => true,
                "false" => false,
                _ => return Err("installer-running-intent-invalid".into()),
            };
            update::installer_rollback_swap_while_locked(&root, &executable, was_running).await
        }
        ["installer", "start-and-verify", "lock-held"] => {
            update::installer_start_and_verify_while_locked(&root, &executable).await
        }
        ["installer", "guard", nonce] => {
            update::run_installer_guard(&root, &executable, nonce).await
        }
        ["installer", "finish", nonce] => update::finish_installer_guard(&root, nonce),
        ["uninstall"] => {
            let _lock = update::try_lock(&root)?;
            risunest_sync_manager::lifecycle::stop(&root, &client).await?;
            update::remove_schedule_while_locked(&root, &manager, &executable)?;
            platform::startup(&root, &executable, "remove")?;
            #[cfg(any(windows, target_os = "macos"))]
            platform::gui::setting(&root, Some(false))?;
            println!("실행 등록을 제거했습니다. 서버 데이터는 유지됩니다.");
            Ok(())
        }
        ["uninstall", "lock-held"] => {
            risunest_sync_manager::lifecycle::stop(&root, &client).await?;
            update::remove_schedule_while_locked(&root, &manager, &executable)?;
            platform::startup(&root, &executable, "remove")?;
            #[cfg(any(windows, target_os = "macos"))]
            platform::gui::setting(&root, Some(false))?;
            println!("실행 등록을 제거했습니다. 서버 데이터는 유지됩니다.");
            Ok(())
        }
        ["autostart", "status"] => {
            let status = platform::startup(&root, &executable, "status")?;
            println!(
                "{}",
                serde_json::to_string(&status).map_err(|_| "startup-status-unavailable")?
            );
            Ok(())
        }
        ["autostart", action @ ("install" | "remove")] => {
            update::set_autostart(&root, &manager, &executable, *action == "install")?;
            println!("자동 실행 설정을 변경했습니다.");
            Ok(())
        }
        ["autostart", action @ ("install" | "remove"), "lock-held"] => {
            update::installer_set_autostart_while_locked(
                &root,
                &manager,
                &executable,
                *action == "install",
            )?;
            println!("자동 실행 설정을 변경했습니다.");
            Ok(())
        }
        ["update", "status"] => {
            let settings = update::load_settings(&root)?;
            let status = update::load_status(&root)?;
            let schedule =
                platform::update_schedule(&root, &manager, &executable, settings.policy, "status")?;
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "settings":settings,
                    "status":status,
                    "schedule":schedule,
                }))
                .map_err(|_| "update-status-unavailable")?
            );
            Ok(())
        }
        ["update", "schedule", "reconcile"] => {
            update::reconcile_schedule(&root, &manager, &executable)
        }
        ["update", "schedule", "reconcile", "lock-held"] => {
            update::reconcile_schedule_while_locked(&root, &manager, &executable)
        }
        ["update", "schedule", "remove", "lock-held"] => {
            update::remove_schedule_while_locked(&root, &manager, &executable)
        }
        ["update", "policy", policy] => {
            let policy = update::UpdatePolicy::parse(policy)?;
            let value = update::set_policy(&root, &manager, &executable, policy)?;
            println!(
                "{}",
                serde_json::to_string(&value).map_err(|_| "update-settings-unavailable")?
            );
            Ok(())
        }
        ["update", "policy", policy, "lock-held"] => {
            let policy = update::UpdatePolicy::parse(policy)?;
            let value =
                update::installer_set_policy_while_locked(&root, &manager, &executable, policy)?;
            println!(
                "{}",
                serde_json::to_string(&value).map_err(|_| "update-settings-unavailable")?
            );
            Ok(())
        }
        ["update", mode @ ("check" | "scheduled")] => {
            let mode = if *mode == "scheduled" {
                update::RunMode::Scheduled
            } else {
                update::RunMode::Manual
            };
            let outcome = update::run(&root, &executable, mode).await?;
            println!(
                "{}",
                serde_json::to_string(&outcome).map_err(|_| "update-status-unavailable")?
            );
            Ok(())
        }
        ["update", "helper", parent, install] => {
            let parent = parent.parse::<u32>().map_err(|_| "update-parent-invalid")?;
            let install = PathBuf::from(install);
            if !install.is_absolute() {
                return Err("managed-install-path-invalid".into());
            }
            let outcome = update::run_helper(&root, &executable, parent, &install).await?;
            println!(
                "{}",
                serde_json::to_string(&outcome).map_err(|_| "update-status-unavailable")?
            );
            Ok(())
        }
        ["update", "helper", parent, install, "--scheduled-task", task] => {
            let parent = parent.parse::<u32>().map_err(|_| "update-parent-invalid")?;
            let install = PathBuf::from(install);
            if !install.is_absolute() {
                return Err("managed-install-path-invalid".into());
            }
            let result = update::run_helper(&root, &executable, parent, &install).await;
            let cleanup = platform::finish_update_helper(&root, task);
            let outcome = result?;
            cleanup?;
            println!(
                "{}",
                serde_json::to_string(&outcome).map_err(|_| "update-status-unavailable")?
            );
            Ok(())
        }
        ["update", "recover-helper", parent, install] => {
            let parent = parent.parse::<u32>().map_err(|_| "update-parent-invalid")?;
            let install = PathBuf::from(install);
            if !install.is_absolute() {
                return Err("managed-install-path-invalid".into());
            }
            let outcome = update::run_recovery_helper(&root, &executable, parent, &install).await?;
            println!(
                "{}",
                serde_json::to_string(&outcome).map_err(|_| "update-status-unavailable")?
            );
            Ok(())
        }
        ["update", "recover-helper", parent, install, "--scheduled-task", task] => {
            let parent = parent.parse::<u32>().map_err(|_| "update-parent-invalid")?;
            let install = PathBuf::from(install);
            if !install.is_absolute() {
                return Err("managed-install-path-invalid".into());
            }
            let result = update::run_recovery_helper(&root, &executable, parent, &install).await;
            let cleanup = platform::finish_update_helper(&root, task);
            let outcome = result?;
            cleanup?;
            println!(
                "{}",
                serde_json::to_string(&outcome).map_err(|_| "update-status-unavailable")?
            );
            Ok(())
        }
        _ => Err("invalid-command".into()),
    }
}

async fn start_server(
    root: &std::path::Path,
    executable: &std::path::Path,
    client: &Client,
) -> Result<()> {
    if client.status().await.is_ok() {
        return Ok(());
    }
    platform::start(root, executable)?;
    for _ in 0..50 {
        if client.status().await.is_ok() {
            println!("서버 실행 중");
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    Err("server-not-ready".into())
}
