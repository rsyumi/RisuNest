use risunest_sync_manager::platform::gui as gui_startup;
use risunest_sync_manager::{client::Client, platform, update, Result};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::Duration,
};
use tauri::{Manager, State};

#[cfg(windows)]
mod snap;

/// Opens the native window menu for the title bar drawn by the webview.
#[tauri::command]
fn window_system_menu(window: tauri::WebviewWindow, x: f64, y: f64) {
    #[cfg(windows)]
    {
        let target = window.as_ref().window();
        let _ = window.run_on_main_thread(move || snap::show_system_menu(&target, Some((x, y))));
    }
    #[cfg(not(windows))]
    let _ = (window, x, y);
}

struct Context {
    root: PathBuf,
    executable: PathBuf,
    client: Client,
    updates: UpdateCoordination,
}

struct UpdateCoordination {
    activity: Mutex<Option<update::ActivityGuard>>,
    handoff_lock: Mutex<Option<update::UpdateLock>>,
}

impl UpdateCoordination {
    fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            activity: Mutex::new(Some(update::ActivityGuard::start(root)?)),
            handoff_lock: Mutex::new(None),
        })
    }

    fn begin(&self, root: &Path) -> Result<update::UpdateLock> {
        let lock = update::try_lock(root)?;
        let mut activity = self
            .activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if activity.take().is_none() {
            return Err("update-handoff-in-progress".into());
        }
        Ok(lock)
    }

    fn restore(&self, root: &Path, lock: update::UpdateLock) -> Result<()> {
        let guard = match update::ActivityGuard::start(root) {
            Ok(guard) => guard,
            Err(error) => {
                self.retain_until_exit(lock)?;
                return Err(error);
            }
        };
        let mut activity = self
            .activity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if activity.replace(guard).is_some() {
            return Err("update-activity-unavailable".into());
        }
        drop(activity);
        drop(lock);
        Ok(())
    }

    fn retain_until_exit(&self, lock: update::UpdateLock) -> Result<()> {
        let mut handoff_lock = self
            .handoff_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if handoff_lock.is_some() {
            return Err("update-handoff-in-progress".into());
        }
        *handoff_lock = Some(lock);
        Ok(())
    }
}
fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn reconcile_schedule_environment(
    policy: update::UpdatePolicy,
    mut status: impl FnMut() -> Result<platform::UpdateScheduleStatus>,
    reconcile: impl FnOnce() -> Result<()>,
) -> (Option<platform::UpdateScheduleStatus>, Option<String>) {
    let current = match status() {
        Ok(status) => status,
        Err(error) => return (None, Some(error)),
    };
    let matches_policy = if policy == update::UpdatePolicy::Off {
        !current.registered && !current.enabled
    } else {
        current.registered && current.enabled && current.action_matches
    };
    if matches_policy {
        return (Some(current), None);
    }
    if let Err(error) = reconcile() {
        return (Some(current), Some(error));
    }
    match status() {
        Ok(status) => (Some(status), None),
        Err(error) => (None, Some(error)),
    }
}

fn update_mode_for_request(
    automatic: bool,
    policy: update::UpdatePolicy,
) -> Option<update::RunMode> {
    if automatic && policy != update::UpdatePolicy::Automatic {
        None
    } else {
        Some(update::RunMode::Manual)
    }
}

fn arm_exit_watchdog(delay: Duration, exit: impl FnOnce() + Send + 'static) -> std::io::Result<()> {
    thread::Builder::new()
        .name("risunest-sync-gui-exit-watchdog".into())
        .spawn(move || {
            thread::sleep(delay);
            exit();
        })
        .map(|_| ())
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
    Err(std::fs::read_to_string(ctx.root.join("startup-error.txt"))
        .unwrap_or_else(|_| "server-not-ready".into()))
}
#[tauri::command]
async fn manager_environment(ctx: State<'_, Context>) -> Result<Value> {
    let root = ctx.root.clone();
    let executable = ctx.executable.clone();
    tauri::async_runtime::spawn_blocking(move||{
        let startup=platform::startup(&root,&executable,"status");
        let (startup,error)=match startup{Ok(s)=>(Some(s),None),Err(e)=>(None,Some(e))};
        let cloudflared=executable.with_file_name(if cfg!(windows){"cloudflared.exe"}else{"cloudflared"});
        let update_settings=risunest_sync_manager::update::load_settings(&root)?;
        let update_status=risunest_sync_manager::update::load_status(&root)?;
        let (update_schedule,update_schedule_error)=match platform::manager_executable(){
            Ok(manager)=>reconcile_schedule_environment(
                update_settings.policy,
                || platform::update_schedule(&root,&manager,&executable,update_settings.policy,"status"),
                || risunest_sync_manager::update::reconcile_schedule(&root,&manager,&executable),
            ),
            Err(error)=>(None,Some(error)),
        };
        Ok(json!({"network":risunest_sync_server::config::NetworkSettings::load(&root).map_err(|e| e.code.to_owned())?,"platform":std::env::consts::OS,"dataDir":path_text(&root),"cloudflared":path_text(&cloudflared),"startup":startup,"startupError":error,"trayStartup":gui_startup::setting(&root,None)?,"updateSettings":update_settings,"updateStatus":update_status,"updateSchedule":update_schedule,"updateScheduleError":update_schedule_error}))
    }).await.map_err(|_|"environment-unavailable")?
}
#[tauri::command]
async fn manager_network(ctx: State<'_, Context>, settings: Value) -> Result<()> {
    let settings: risunest_sync_server::config::NetworkSettings =
        serde_json::from_value(settings).map_err(|_| "invalid-network-settings")?;
    let root = ctx.root.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _lock = update::try_lock(&root)?;
        if root.join("manager-update/transaction.json").exists() {
            return Err("update-recovery-required".into());
        }
        settings.save(&root).map_err(|e| e.code.to_owned())
    })
    .await
    .map_err(|_| "network-settings-save-failed")?
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
    tauri::async_runtime::spawn_blocking(move || {
        let manager = platform::manager_executable()?;
        risunest_sync_manager::update::set_autostart(
            &root,
            &manager,
            &executable,
            action == "install",
        )
    })
    .await
    .map_err(|_| "startup-operation-failed")?
}
#[tauri::command]
async fn manager_update_policy(ctx: State<'_, Context>, policy: String) -> Result<Value> {
    let root = ctx.root.clone();
    let server = ctx.executable.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let policy = risunest_sync_manager::update::UpdatePolicy::parse(&policy)?;
        let manager = platform::manager_executable()?;
        let value = risunest_sync_manager::update::set_policy(&root, &manager, &server, policy)?;
        serde_json::to_value(value).map_err(|_| "update-settings-unavailable".into())
    })
    .await
    .map_err(|_| "update-settings-unavailable")?
}
#[tauri::command]
async fn manager_update_check(
    app: tauri::AppHandle,
    ctx: State<'_, Context>,
    automatic: bool,
) -> Result<Value> {
    let lock = ctx.updates.begin(&ctx.root)?;
    let mode = if automatic {
        let settings = match update::load_settings(&ctx.root) {
            Ok(settings) => settings,
            Err(error) => {
                ctx.updates.restore(&ctx.root, lock)?;
                return Err(error);
            }
        };
        let Some(mode) = update_mode_for_request(true, settings.policy) else {
            ctx.updates.restore(&ctx.root, lock)?;
            return serde_json::to_value(update::RunOutcome::Skipped)
                .map_err(|_| "update-status-unavailable".into());
        };
        mode
    } else {
        update::RunMode::Manual
    };
    let outcome =
        risunest_sync_manager::update::run_while_locked(&ctx.root, &ctx.executable, mode, &lock)
            .await;
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            ctx.updates.restore(&ctx.root, lock)?;
            return Err(error);
        }
    };
    if matches!(
        outcome,
        risunest_sync_manager::update::RunOutcome::Started(_)
    ) {
        if let Err(error) = ctx.updates.restore(&ctx.root, lock) {
            if arm_exit_watchdog(Duration::from_secs(5), || std::process::exit(1)).is_err() {
                std::process::exit(1);
            }
            app.exit(1);
            return Err(error);
        }
        tauri::async_runtime::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            app.exit(0);
        });
    } else {
        ctx.updates.restore(&ctx.root, lock)?;
    }
    let value = serde_json::to_value(&outcome).map_err(|_| "update-status-unavailable")?;
    Ok(value)
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
    let updates = UpdateCoordination::new(&root).expect("update activity unavailable");
    let mut tauri_context = tauri::generate_context!();
    let main_window = if let Some(data_directory) =
        platform::webview_data_dir().expect("WebView data directory unavailable")
    {
        let index = tauri_context
            .config_mut()
            .app
            .windows
            .iter()
            .position(|window| window.label == "main")
            .expect("main window missing");
        Some((tauri_context.config_mut().app.windows.remove(index), data_directory))
    } else {
        None
    };
    tauri::Builder::default()
        .manage(Context {
            root,
            executable,
            client,
            updates,
        })
        .invoke_handler(tauri::generate_handler![
            manager_status,
            manager_mutate,
            manager_start,
            manager_environment,
            manager_network,
            manager_startup,
            manager_update_policy,
            manager_update_check,
            manager_tray_startup,
            manager_request_id,
            manager_qr,
            window_system_menu
        ])
        .setup(move |app| {
            if let Some((config, data_directory)) = &main_window {
                tauri::WebviewWindowBuilder::from_config(app, config)?
                    .data_directory(data_directory.clone())
                    .build()?;
            }
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
            if let Some(window) = app.get_webview_window("main") {
                // The frontend draws the title bar over the acrylic backdrop on Windows.
                #[cfg(windows)]
                {
                    window.set_decorations(false)?;
                    snap::attach(&window.as_ref().window());
                }
                if !tray {
                    window.show()?;
                }
            }
            Ok(())
        })
        .on_window_event(move |window, event| match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if tray {
                    api.prevent_close();
                    let _ = window.hide();
                } else {
                    window.app_handle().exit(0);
                }
            }
            #[cfg(windows)]
            tauri::WindowEvent::Resized(_) | tauri::WindowEvent::ScaleFactorChanged { .. } => {
                snap::layout(window)
            }
            _ => (),
        })
        .run(tauri_context)
        .expect("sync manager runtime failed");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, ffi::OsString};

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "risunest-sync-gui-test-{}",
                manager_request_id().unwrap()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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

    #[test]
    fn update_handoff_holds_the_update_lock_before_presence_is_released() {
        let root = TestRoot::new();
        let coordination = UpdateCoordination::new(root.path()).unwrap();
        assert!(update::has_active_activity(root.path()).unwrap());

        let lock = coordination.begin(root.path()).unwrap();
        assert!(!update::has_active_activity(root.path()).unwrap());
        let contender_root = root.path().to_owned();
        assert_eq!(
            std::thread::spawn(move || update::try_lock(&contender_root).unwrap_err())
                .join()
                .unwrap(),
            "update-already-running"
        );

        coordination.restore(root.path(), lock).unwrap();
        assert!(update::has_active_activity(root.path()).unwrap());
        drop(update::try_lock(root.path()).unwrap());
    }

    #[test]
    fn failed_presence_restore_keeps_the_open_gui_protected() {
        let root = TestRoot::new();
        let coordination = UpdateCoordination::new(root.path()).unwrap();
        let lock = coordination.begin(root.path()).unwrap();
        let blocked_root = root.path().join("unwritable-root");
        std::fs::write(&blocked_root, "synthetic filesystem failure").unwrap();

        assert!(coordination.restore(&blocked_root, lock).is_err());
        assert_eq!(
            update::try_lock(root.path()).unwrap_err(),
            "update-already-running"
        );
        drop(coordination);
        drop(update::try_lock(root.path()).unwrap());
    }

    #[test]
    fn started_update_restores_presence_before_releasing_the_lock_to_the_helper() {
        let root = TestRoot::new();
        let coordination = UpdateCoordination::new(root.path()).unwrap();
        let lock = coordination.begin(root.path()).unwrap();
        assert!(!update::has_active_activity(root.path()).unwrap());
        coordination.restore(root.path(), lock).unwrap();
        assert!(update::has_active_activity(root.path()).unwrap());
        drop(update::try_lock(root.path()).unwrap());
    }

    #[test]
    fn automatic_handoff_rechecks_policy_after_taking_the_update_lock() {
        assert_eq!(
            update_mode_for_request(true, update::UpdatePolicy::Automatic),
            Some(update::RunMode::Manual)
        );
        assert_eq!(
            update_mode_for_request(true, update::UpdatePolicy::Notify),
            None
        );
        assert_eq!(
            update_mode_for_request(true, update::UpdatePolicy::Off),
            None
        );
        assert_eq!(
            update_mode_for_request(false, update::UpdatePolicy::Off),
            Some(update::RunMode::Manual)
        );
    }

    #[test]
    fn failed_handoff_exit_watchdog_runs_within_its_bound() {
        let (send, receive) = std::sync::mpsc::channel();
        arm_exit_watchdog(Duration::from_millis(10), move || send.send(()).unwrap()).unwrap();
        receive.recv_timeout(Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn schedule_reconciliation_failure_keeps_status_available_and_can_retry() {
        let attempts = Cell::new(0);
        let reconcile = || -> Result<()> {
            attempts.set(attempts.get() + 1);
            if attempts.get() == 1 {
                Err("update-already-running".into())
            } else {
                Ok(())
            }
        };
        let missing = || {
            Ok(platform::UpdateScheduleStatus {
                registered: false,
                enabled: false,
                action_matches: false,
            })
        };
        let (status, error) =
            reconcile_schedule_environment(update::UpdatePolicy::Automatic, missing, reconcile);
        assert!(!status.unwrap().registered);
        assert_eq!(error.as_deref(), Some("update-already-running"));

        let status_reads = Cell::new(0);
        let (status, error) = reconcile_schedule_environment(
            update::UpdatePolicy::Automatic,
            || {
                status_reads.set(status_reads.get() + 1);
                Ok(platform::UpdateScheduleStatus {
                    registered: status_reads.get() == 2,
                    enabled: status_reads.get() == 2,
                    action_matches: status_reads.get() == 2,
                })
            },
            reconcile,
        );
        let status = status.unwrap();
        assert!(status.registered && status.enabled && status.action_matches);
        assert_eq!(error, None);
        assert_eq!(attempts.get(), 2);
    }

    #[test]
    fn schedule_status_failure_is_reported_without_aborting_environment_work() {
        let (status, error) = reconcile_schedule_environment(
            update::UpdatePolicy::Automatic,
            || Err("user-service-unavailable".into()),
            || panic!("failed status must not attempt scheduler mutation"),
        );
        assert!(status.is_none());
        assert_eq!(error.as_deref(), Some("user-service-unavailable"));
    }

    #[test]
    fn healthy_or_disabled_schedule_is_not_reinstalled_during_environment_polling() {
        let reconciliations = Cell::new(0);
        let (status, error) = reconcile_schedule_environment(
            update::UpdatePolicy::Notify,
            || {
                Ok(platform::UpdateScheduleStatus {
                    registered: true,
                    enabled: true,
                    action_matches: true,
                })
            },
            || {
                reconciliations.set(reconciliations.get() + 1);
                Ok(())
            },
        );
        assert!(status.unwrap().action_matches);
        assert_eq!(error, None);
        assert_eq!(reconciliations.get(), 0);

        let (status, error) = reconcile_schedule_environment(
            update::UpdatePolicy::Off,
            || {
                Ok(platform::UpdateScheduleStatus {
                    registered: false,
                    enabled: false,
                    action_matches: false,
                })
            },
            || {
                reconciliations.set(reconciliations.get() + 1);
                Ok(())
            },
        );
        assert!(!status.unwrap().registered);
        assert_eq!(error, None);
        assert_eq!(reconciliations.get(), 0);
    }

    #[test]
    fn off_policy_removes_a_registered_schedule_once() {
        let status_reads = Cell::new(0);
        let reconciliations = Cell::new(0);
        let (status, error) = reconcile_schedule_environment(
            update::UpdatePolicy::Off,
            || {
                status_reads.set(status_reads.get() + 1);
                Ok(platform::UpdateScheduleStatus {
                    registered: status_reads.get() == 1,
                    enabled: status_reads.get() == 1,
                    action_matches: true,
                })
            },
            || {
                reconciliations.set(reconciliations.get() + 1);
                Ok(())
            },
        );
        let status = status.unwrap();
        assert!(!status.registered && !status.enabled);
        assert_eq!(error, None);
        assert_eq!(reconciliations.get(), 1);
    }

    #[test]
    fn relocated_manager_schedule_is_reconciled_and_rechecked() {
        let status_reads = Cell::new(0);
        let reconciliations = Cell::new(0);
        let (status, error) = reconcile_schedule_environment(
            update::UpdatePolicy::Automatic,
            || {
                status_reads.set(status_reads.get() + 1);
                Ok(platform::UpdateScheduleStatus {
                    registered: true,
                    enabled: true,
                    action_matches: status_reads.get() == 2,
                })
            },
            || {
                reconciliations.set(reconciliations.get() + 1);
                Ok(())
            },
        );
        assert!(status.unwrap().action_matches);
        assert_eq!(error, None);
        assert_eq!(reconciliations.get(), 1);
        assert_eq!(status_reads.get(), 2);
    }
}
