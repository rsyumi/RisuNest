use std::path::PathBuf;

/// Resolve native storage only from the platform-owned application directory.
pub(crate) fn resolve<R: tauri::Runtime>(app: &impl tauri::Manager<R>) -> tauri::Result<PathBuf> {
    let root = app.path().app_data_dir()?;
    #[cfg(target_os = "android")]
    return resolve_android_root(&root).map_err(Into::into);
    #[cfg(not(target_os = "android"))]
    Ok(root)
}

#[cfg(any(test, target_os = "android"))]
fn resolve_android_root(root: &std::path::Path) -> std::io::Result<PathBuf> {
    use std::{fs, io};

    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Android application data root must be absolute",
        ));
    }
    let metadata = fs::symlink_metadata(root)?;
    if crate::trust_boundary::is_link_like(&metadata) || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Android application data root must be a real directory",
        ));
    }

    // Android's app mount namespace can alias /data/user/0 to /data/data.
    // Only the OS-provided root may cross such parents. CAS still rejects
    // links in every path it receives, including all app-owned descendants.
    fs::canonicalize(root)
}

#[cfg(test)]
mod tests;
