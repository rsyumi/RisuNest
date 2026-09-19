use super::{invalid_source_error, NativeJobError};
use std::fs::File;
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use windows_sys::Win32::{
    Foundation::{GENERIC_READ, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{
        GetFileInformationByHandleEx, ReOpenFile, FileAttributeTagInfo,
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_ATTRIBUTE_TAG_INFO,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    },
};

fn supported_cloud_tag(tag: u32) -> bool {
    const CLOUD: u32 = 0x9000_001a;
    const CLOUD_MASK: u32 = 0x0000_f000;
    tag & !CLOUD_MASK == CLOUD
}

fn attributes(file: &File) -> Result<FILE_ATTRIBUTE_TAG_INFO, NativeJobError> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    let ok = unsafe { GetFileInformationByHandleEx(file.as_raw_handle(), FileAttributeTagInfo,
        (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(), std::mem::size_of_val(&info) as u32) };
    if ok == 0 {
        return Err(invalid_source_error("The selected file's cloud status could not be checked. Copy it to a local folder and import it again."));
    }
    Ok(info)
}

pub(super) fn reopen_cloud_source(file: File) -> Result<File, NativeJobError> {
    if !supported_cloud_tag(attributes(&file)?.ReparseTag) {
        return Err(invalid_source_error("The selected file uses an unsupported link or cloud provider. Copy it to a local folder and import it again."));
    }
    // Reopen the validated object, never a path that can change between opens.
    let handle = unsafe { ReOpenFile(file.as_raw_handle(), GENERIC_READ,
        FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE, 0) };
    if handle == INVALID_HANDLE_VALUE {
        return Err(invalid_source_error("The selected cloud file could not be opened. Download a local copy and import it again."));
    }
    let reopened = unsafe { File::from_raw_handle(handle) };
    let info = attributes(&reopened)?;
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 && !supported_cloud_tag(info.ReparseTag) {
        return Err(invalid_source_error("The selected file changed to an unsupported link."));
    }
    Ok(reopened)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cloud_policy_accepts_only_the_known_nonredirecting_family() {
        for variant in 0..=15 { assert!(supported_cloud_tag(0x9000_001a | (variant << 12))); }
        for tag in [0, 0xa000_0003, 0xa000_000c, 0x8000_0021, 0x9000_001b, 0x9001_001a, 0xb000_001a] {
            assert!(!supported_cloud_tag(tag), "unexpected tag: {tag:x}");
        }
    }
    #[test]
    fn ordinary_handle_is_not_misclassified_as_cloud() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("source");
        std::fs::write(&path, b"synthetic").unwrap();
        let file = File::open(path).unwrap();
        assert_eq!(attributes(&file).unwrap().FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT, 0);
        assert!(reopen_cloud_source(file).is_err());
    }
}
