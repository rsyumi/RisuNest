use serde::Serialize;
use tauri::AppHandle;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct IntegrationState {
    available: bool,
    registered: bool,
    replaces_existing: bool,
    token: String,
    error: Option<String>,
}

#[cfg(any(test, target_os = "linux"))]
fn permitted(debug: bool, agent: bool, bundle: Option<&str>) -> bool {
    !debug && !agent && bundle == Some("appimage")
}

#[cfg(any(test, target_os = "linux"))]
fn registration_status(handler: &str, name: &str, desktop: Option<&str>, expected_exec: &str) -> (bool, bool) {
    let same_path = desktop.is_some_and(|value| {
        let mut entry = false;
        let mut executable = None;
        for line in value.lines() {
            if line.starts_with('[') { entry = line == "[Desktop Entry]"; }
            if entry { if let Some(value) = line.strip_prefix("Exec=") { executable = Some(value); } }
        }
        executable == Some(expected_exec)
    });
    (handler == name && same_path,
        (!handler.is_empty() && handler != name) || (desktop.is_some() && !same_path))
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::{fs, path::PathBuf, process::Command};
    use tauri::Manager;
    use tauri_plugin_deep_link::DeepLinkExt;
    const MIME: &str = "x-scheme-handler/risunestlocal";

    pub(super) fn available(app: &AppHandle) -> bool {
        let agent = app.config().plugins.0.get("risunest")
            .and_then(|value| value.get("agent")).and_then(|value| value.as_bool()).unwrap_or(false);
        permitted(cfg!(debug_assertions), agent,
            tauri::utils::platform::bundle_type().as_ref().map(|value| value.to_string()).as_deref())
            && app.env().appimage.is_some()
    }

    fn paths(app: &AppHandle) -> Result<(PathBuf, String, String), String> {
        let image = PathBuf::from(app.env().appimage.ok_or("AppImage path is unavailable")?);
        let image_text = image.to_str().ok_or("AppImage path is not valid UTF-8")?;
        if !image.is_absolute() || !image.is_file() || image_text.chars().any(|ch| "\"\\`$%\n\r".contains(ch)) {
            return Err("Move the AppImage to a regular local path without quotes or special characters, then try again.".into());
        }
        let binary = tauri::utils::platform::current_exe().map_err(|error| error.to_string())?;
        let name = format!("{}-handler.desktop", binary.file_name().ok_or("AppImage executable is unavailable")?.to_string_lossy());
        let target = app.path().data_dir().map_err(|error| error.to_string())?.join("applications").join(&name);
        Ok((target, name, format!("\"{image_text}\" %u")))
    }

    pub(super) fn state(app: &AppHandle) -> Result<IntegrationState, String> {
        let (target, name, expected_exec) = paths(app)?;
        let output = Command::new("xdg-mime").args(["query", "default", MIME]).output().map_err(|error| error.to_string())?;
        if !output.status.success() { return Err("The desktop URL handler could not be read.".into()); }
        let handler = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
        let handler = handler.trim();
        let desktop = match fs::read_to_string(&target) {
            Ok(value) => Some(value),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.to_string()),
        };
        let (registered, replaces_existing) = registration_status(handler, &name, desktop.as_deref(), &expected_exec);
        let token = hex::encode(Sha256::digest(format!("{handler}\0{}", desktop.as_deref().unwrap_or(""))));
        Ok(IntegrationState { available: true, registered, replaces_existing, token, error: None })
    }

    pub(super) fn register(app: &AppHandle, token: &str, replace_existing: bool) -> Result<(), String> {
        if !available(app) { return Err("AppImage integration is unavailable in this build.".into()); }
        let current = state(app)?;
        if current.token != token { return Err("The URL handler changed. Try again.".into()); }
        if current.replaces_existing && !replace_existing { return Err("Replacing the existing URL handler requires confirmation.".into()); }
        app.deep_link().register("risunestlocal").map_err(|error| error.to_string())?;
        if !state(app)?.registered { return Err("The desktop URL handler could not be registered.".into()); }
        Ok(())
    }
}

#[tauri::command]
pub(crate) async fn appimage_integration_state(_app: AppHandle) -> Result<IntegrationState, String> {
    let unavailable = IntegrationState { available: false, registered: false, replaces_existing: false, token: String::new(), error: None };
    #[cfg(target_os = "linux")]
    if linux::available(&_app) {
        return tauri::async_runtime::spawn_blocking(move || match linux::state(&_app) {
            Ok(state) => state,
            Err(error) => IntegrationState { available: true, error: Some(error), ..unavailable },
        }).await.map_err(|error| error.to_string());
    }
    Ok(unavailable)
}

#[tauri::command]
pub(crate) async fn appimage_integration_register(_app: AppHandle, token: String, replace_existing: bool) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    return tauri::async_runtime::spawn_blocking(move || linux::register(&_app, &token, replace_existing))
        .await.map_err(|error| error.to_string())?;
    #[cfg(not(target_os = "linux"))]
    { let _ = (token, replace_existing); Err("AppImage integration is unavailable on this platform.".into()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_compares_both_default_handler_and_launch_path() {
        let same = "[Desktop Entry]\nExec=\"/a/app.AppImage\" %u\n";
        let expected = "\"/a/app.AppImage\" %u";
        assert_eq!(registration_status("", "app-handler.desktop", None, expected), (false, false));
        assert_eq!(registration_status("other.desktop", "app-handler.desktop", Some(same), expected), (false, true));
        assert_eq!(registration_status("app-handler.desktop", "app-handler.desktop", Some(same), expected), (true, false));
        assert_eq!(registration_status("app-handler.desktop", "app-handler.desktop", Some(same), "\"/moved/app.AppImage\" %u"), (false, true));
        let duplicate = format!("{same}Exec=other %u\n");
        assert_eq!(registration_status("app-handler.desktop", "app-handler.desktop", Some(&duplicate), expected), (false, true));
    }
    #[test]
    fn integration_requires_a_production_appimage() {
        assert!(permitted(false, false, Some("appimage")));
        assert!(!permitted(true, false, Some("appimage")));
        assert!(!permitted(false, true, Some("appimage")));
        assert!(!permitted(false, false, Some("deb")));
        assert!(!permitted(false, false, None));
    }
}
