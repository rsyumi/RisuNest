use std::path::PathBuf;

/// Resolve native storage only from the platform-owned application directory.
pub(crate) fn resolve<R: tauri::Runtime>(app: &impl tauri::Manager<R>) -> tauri::Result<PathBuf> {
    let root = app.path().app_data_dir()?;
    #[cfg(any(target_os = "android", target_os = "linux"))]
    return resolve_platform_root(&root, cfg!(target_os = "linux")).map_err(Into::into);
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    Ok(root)
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
