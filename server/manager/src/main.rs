use risunest_sync_manager::{client::Client, platform, tui, Result};
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
                println!("risunest-sync-manager [--data-dir PATH] [--server EXECUTABLE] [status|start|stop|autostart install|autostart remove]\n명령을 생략하면 관리 메뉴를 엽니다.");
                return Ok(());
            }
            _ => command.push(arg),
        }
    }
    if !root.is_absolute() {
        return Err("absolute-data-dir-required".into());
    }
    let client = Client::new(root.clone())?;
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
        ["start"] => {
            if client.status().await.is_ok() {
                return Ok(());
            }
            platform::start(&root, &executable)?;
            for _ in 0..50 {
                if client.status().await.is_ok() {
                    println!("서버 실행 중");
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err("server-not-ready".into())
        }
        ["stop"] => {
            risunest_sync_manager::lifecycle::stop(&root, &client).await?;
            println!("서버 중지됨");
            Ok(())
        }
        ["prepare-update"] => risunest_sync_manager::lifecycle::stop(&root, &client).await,
        ["uninstall"] => {
            risunest_sync_manager::lifecycle::stop(&root, &client).await?;
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
            platform::startup(&root, &executable, action)?;
            println!("자동 실행 설정을 변경했습니다.");
            Ok(())
        }
        _ => Err("invalid-command".into()),
    }
}
