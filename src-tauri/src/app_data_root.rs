use std::path::PathBuf;

/// Resolve native storage only from the platform-owned application directory.
pub(crate) fn resolve<R: tauri::Runtime>(app: &impl tauri::Manager<R>) -> tauri::Result<PathBuf> {
    #[cfg(windows)]
    let root = windows_data_root(app.path().data_dir()?, "RisuNestData")?;
    #[cfg(not(windows))]
    let root = app.path().app_data_dir()?;
    #[cfg(any(target_os = "android", target_os = "linux"))]
    return resolve_platform_root(&root, cfg!(target_os = "linux")).map_err(Into::into);
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    Ok(root)
}

#[cfg(desktop)]
pub(crate) fn take_main_window_with_webview_root(
    context: &mut tauri::Context<tauri::Wry>,
) -> std::io::Result<Option<(tauri::utils::config::WindowConfig, PathBuf)>> {
    let Some(root) = desktop_webview_data_root()? else {
        return Ok(None);
    };
    let index = context
        .config_mut()
        .app
        .windows
        .iter()
        .position(|window| window.label == "main")
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "main window missing")
        })?;
    let window = context.config_mut().app.windows.remove(index);
    Ok(Some((window, root)))
}

#[cfg(desktop)]
fn desktop_webview_data_root() -> std::io::Result<Option<PathBuf>> {
    #[cfg(windows)]
    return required_absolute_env("LOCALAPPDATA")
        .and_then(|root| windows_data_root(root, "RisuNestWebViewData"))
        .map(Some);
    #[cfg(target_os = "linux")]
    return linux_data_home(
        std::env::var_os("XDG_DATA_HOME").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
    .map(|root| Some(root.join("risunest-webview")));
    #[cfg(target_os = "macos")]
    Ok(None)
}

#[cfg(all(desktop, windows))]
fn required_absolute_env(name: &str) -> std::io::Result<PathBuf> {
    let root = std::env::var_os(name)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("{name} unavailable"))
        })?;
    Ok(root)
}

#[cfg(any(test, windows))]
fn windows_data_root(root: PathBuf, leaf: &str) -> std::io::Result<PathBuf> {
    root.is_absolute()
        .then(|| root.join(leaf))
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "data root must be absolute")
        })
}

#[cfg(any(test, target_os = "linux"))]
fn linux_data_home(xdg: Option<PathBuf>, home: Option<PathBuf>) -> std::io::Result<PathBuf> {
    if let Some(root) = xdg.filter(|path| path.is_absolute()) {
        return Ok(root);
    }
    home.filter(|path| path.is_absolute())
        .map(|path| path.join(".local/share"))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "user data directory unavailable",
            )
        })
}

#[cfg(any(test, target_os = "android", target_os = "linux"))]
fn resolve_platform_root(root: &std::path::Path, create_missing: bool) -> std::io::Result<PathBuf> {
    use std::{fs, io};

    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Application data root must be absolute",
        ));
    }
    let metadata = match fs::symlink_metadata(root) {
        Err(error) if create_missing && error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root)?;
            fs::symlink_metadata(root)?
        }
        result => result?,
    };
    if crate::trust_boundary::is_link_like(&metadata) || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Application data root must be a real directory",
        ));
    }

    // Android mount aliases and Linux data/home aliases are trusted only
    // above the OS-provided root. CAS still rejects all app-owned links.
    fs::canonicalize(root)
}

#[cfg(test)]
mod tests;
