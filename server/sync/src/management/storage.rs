use crate::{Error, Result};
use serde::Serialize;
use std::{
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageUsage {
    pub measured_at: Option<u64>,
    pub total_bytes: Option<u64>,
    pub data_bytes: u64,
    pub database_bytes: u64,
    pub temporary_bytes: u64,
    pub other_bytes: u64,
    pub available_bytes: Option<u64>,
    pub error: Option<&'static str>,
}

pub fn measure(root: &Path) -> Result<StorageUsage> {
    let started = Instant::now();
    let mut value = StorageUsage::default();
    let mut pending: Vec<PathBuf> = vec![root.to_owned()];
    let mut count = 0u64;
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(dir)? {
            count += 1;
            if count > 1_000_000 || started.elapsed().as_secs() >= 10 {
                return Err(Error::new("storage-measurement-budget-exceeded", 503));
            }
            let entry = entry?;
            let meta = std::fs::symlink_metadata(entry.path())?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if meta.file_attributes() & 0x400 != 0 {
                    return Err(Error::new("unsafe-storage-path", 409));
                }
            }
            if meta.file_type().is_symlink() {
                return Err(Error::new("unsafe-storage-path", 409));
            }
            if meta.is_dir() {
                pending.push(entry.path());
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let path = entry.path();
            let top = path
                .strip_prefix(root)
                .ok()
                .and_then(|p| p.components().next())
                .map(|s| s.as_os_str().to_string_lossy());
            let bucket = match top.as_deref() {
                Some("objects") => &mut value.data_bytes,
                Some("staging") => &mut value.temporary_bytes,
                Some("metadata.sqlite" | "metadata.sqlite-wal" | "metadata.sqlite-shm") => {
                    &mut value.database_bytes
                }
                _ => &mut value.other_bytes,
            };
            *bucket = bucket
                .checked_add(meta.len())
                .ok_or(Error::new("storage-size-overflow", 503))?;
        }
    }
    value.total_bytes = Some(
        value
            .data_bytes
            .checked_add(value.database_bytes)
            .and_then(|n| n.checked_add(value.temporary_bytes))
            .and_then(|n| n.checked_add(value.other_bytes))
            .ok_or(Error::new("storage-size-overflow", 503))?,
    );
    value.measured_at = Some(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| Error::new("clock-unavailable", 503))?
            .as_secs(),
    );
    value.available_bytes = available(root);
    Ok(value)
}

#[cfg(windows)]
fn available(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut bytes = 0;
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            name.as_ptr(),
            &mut bytes,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(bytes)
}
#[cfg(unix)]
fn available(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    stat.f_bavail.checked_mul(stat.f_frsize)
}
