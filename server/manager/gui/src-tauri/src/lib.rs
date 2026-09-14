use risunest_sync_manager::platform::gui as gui_startup;
use risunest_sync_manager::{client::Client, platform, Result};
use serde_json::{json, Value};
use std::{path::{Path, PathBuf}, time::Duration};
use tauri::{Manager, State};

struct Context {
    root: PathBuf,
    executable: PathBuf,
    client: Client,
}
fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
#[tauri::command]
async fn manager_status(app: tauri::AppHandle, ctx: State<'_, Context>) -> Result<Value> {
    let result = ctx.client.status().await;
    if let Some(tray) = app.tray_by_id("manager-tray") {
        let _ = tray.set_tooltip(Some(if result.is_ok() {
            "RisuNest Sync · 서버 실행 중"
        } else {
            "RisuNest Sync · 서버 연결 안 됨"
        }));
    }
    result
}
#[tauri::command]
async fn manager_mutate(ctx: State<'_, Context>, path: String, body: Value) -> Result<Value> {
    ctx.client.mutate(&path, body).await
}
#[tauri::command]
async fn manager_start(ctx: State<'_, Context>) -> Result<()> {
    if ctx.client.status().await.is_ok() {
        return Ok(());
    }
    let root = ctx.root.clone();
    let executable = ctx.executable.clone();
    tauri::async_runtime::spawn_blocking(move || platform::start(&root, &executable))
        .await
        .map_err(|_| "server-start-failed")??;
    for _ in 0..50 {
        if ctx.client.status().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err("server-not-ready".into())
}
#[tauri::command]
async fn manager_environment(ctx: State<'_, Context>) -> Result<Value> {
    let root = ctx.root.clone();
    let executable = ctx.executable.clone();
    tauri::async_runtime::spawn_blocking(move||{
        let startup=platform::startup(&root,&executable,"status");
        let (startup,error)=match startup{Ok(s)=>(Some(s),None),Err(e)=>(None,Some(e))};
        let cloudflared=executable.with_file_name(if cfg!(windows){"cloudflared.exe"}else{"cloudflared"});
        Ok(json!({"platform":std::env::consts::OS,"dataDir":path_text(&root),"cloudflared":path_text(&cloudflared),"startup":startup,"startupError":error,"trayStartup":gui_startup::setting(&root,None)?}))
    }).await.map_err(|_|"environment-unavailable")?
}
#[tauri::command]
async fn manager_startup(
    ctx: State<'_, Context>,
    action: String,
) -> Result<platform::StartupStatus> {
    if !["install", "remove"].contains(&action.as_str()) {
        return Err("invalid-startup-action".into());
    }
    let root = ctx.root.clone();
    let executable = ctx.executable.clone();
    tauri::async_runtime::spawn_blocking(move || platform::startup(&root, &executable, &action))
        .await
        .map_err(|_| "startup-operation-failed")?
}
#[tauri::command]
async fn manager_tray_startup(ctx: State<'_, Context>, enabled: bool) -> Result<()> {
    let root = ctx.root.clone();
    tauri::async_runtime::spawn_blocking(move || {
        gui_startup::setting(&root, Some(enabled)).map(|_| ())
    })
    .await
    .map_err(|_| "startup-operation-failed")?
}
#[tauri::command]
fn manager_request_id() -> Result<String> {
    risunest_sync_server::management::discovery::request_id().map_err(|e| e.code.into())
}
#[tauri::command]
fn manager_qr(uri: String) -> Result<String> {
    risunest_sync_connect::Registration::parse_uri(&uri).map_err(|_| "invalid-registration-uri")?;
    let qr = qrcode::QrCode::with_error_correction_level(uri.as_bytes(), qrcode::EcLevel::M)
        .map_err(|_| "qr-unavailable")?;
    let width = qr.width();
    let mut path = String::new();
    for y in 0..width {
        for x in 0..width {
            if qr[(x, y)] == qrcode::Color::Dark {
                path.push_str(&format!("M{} {}h1v1h-1z", x + 4, y + 4));
            }
        }
    }
    Ok(format!("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {} {}\" shape-rendering=\"crispEdges\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><path d=\"{path}\" fill=\"black\"/></svg>",width+8,width+8))
}

pub fn run() {
    let mut root = platform::default_data_dir().expect("user data directory unavailable");
    let mut args = std::env::args().skip(1);
    let mut tray = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => root = PathBuf::from(args.next().expect("data-dir required")),
            "--tray" => tray = true,
            _ => (),
        }
    }
    assert!(root.is_absolute(), "absolute data directory required");
    let executable = platform::server_executable().expect("server path unavailable");
    let client = Client::new(root.clone()).expect("management client unavailable");
    tauri::Builder::default()
        .manage(Context {
            root,
            executable,
            client,
        })
        .invoke_handler(tauri::generate_handler![
            manager_status,
            manager_mutate,
            manager_start,
            manager_environment,
            manager_startup,
            manager_tray_startup,
            manager_request_id,
            manager_qr
        ])
        .setup(move |app| {
            use tauri::{
                menu::{Menu, MenuItem},
                tray::TrayIconBuilder,
            };
            let show = MenuItem::with_id(app, "show", "관리 화면 열기", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "관리 앱 종료", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::with_id("manager-tray")
                .tooltip("RisuNest Sync")
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => (),
                })
                .build(app)?;
            if !tray {
                if let Some(window) = app.get_webview_window("main") {
                    window.show()?;
                }
            }
            Ok(())
        })
        .on_window_event(move |window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if tray {
                    api.prevent_close();
                    let _ = window.hide();
                } else {
                    window.app_handle().exit(0);
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("sync manager runtime failed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn environment_paths_are_json_safe_when_the_os_path_is_not_unicode() {
        #[cfg(windows)]
        let value = {
            use std::os::windows::ffi::OsStringExt;
            OsString::from_wide(&[0xd800])
        };
        #[cfg(unix)]
        let value = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(vec![0xff])
        };
        let path = PathBuf::from(value);
        assert!(json!({"path":path_text(&path)})["path"].is_string());
    }
}
