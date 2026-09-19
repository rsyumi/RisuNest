use std::{fs::File, io::Write, path::Path};

use crate::{Error, Result};

pub(crate) fn install(path: &Path, bytes: &[u8]) -> Result<()> {
    replace_with(path, |candidate| {
        #[cfg(all(unix, feature = "zip"))]
        if infer::archive::is_gz(bytes) {
            let decoder = flate2::read::GzDecoder::new(bytes);
            let mut archive = tar::Archive::new(decoder);
            for entry in archive.entries()? {
                let mut entry = entry?;
                if entry.header().entry_type().is_file()
                    && entry.path()?.extension() == Some(std::ffi::OsStr::new("AppImage"))
                {
                    std::io::copy(&mut entry, candidate)?;
                    return Ok(());
                }
            }
            return Err(Error::BinaryNotFoundInArchive);
        }
        candidate.write_all(bytes)?;
        Ok(())
    })
}

fn replace_with(path: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(std::io::Error::other("installed AppImage must be a regular file").into());
    }
    let parent = path.parent().ok_or(Error::FailedToDetermineExtractPath)?;
    #[cfg(unix)]
    let directory = File::open(parent)?;
    let mut candidate = tempfile::Builder::new()
        .prefix(".risunest-update-")
        .suffix(".AppImage")
        .tempfile_in(parent)?;
    write(candidate.as_file_mut())?;
    candidate.as_file().set_permissions(metadata.permissions())?;
    candidate.as_file().sync_all()?;
    candidate.persist(path).map_err(|error| error.error)?;
    #[cfg(unix)]
    directory.sync_all().map_err(|error| {
        std::io::Error::other(format!(
            "updated AppImage was installed, but its directory could not be synchronized: {error}"
        ))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_candidate_keeps_the_installed_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("RisuNest.AppImage");
        std::fs::write(&path, b"installed").unwrap();
        let result = replace_with(&path, |candidate| {
            candidate.write_all(b"partial")?;
            assert_eq!(std::fs::read(&path).unwrap(), b"installed");
            Err(std::io::Error::other("synthetic write failure").into())
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"installed");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn publishes_a_complete_candidate_without_removing_the_launch_path_first() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("RisuNest.AppImage");
        std::fs::write(&path, b"installed").unwrap();
        replace_with(&path, |candidate| {
            candidate.write_all(b"complete update")?;
            assert_eq!(std::fs::read(&path).unwrap(), b"installed");
            Ok(())
        }).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete update");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn raw_install_replaces_only_the_selected_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("RisuNest.AppImage");
        let sibling = root.path().join("unrelated");
        std::fs::write(&path, b"installed").unwrap();
        std::fs::write(&sibling, b"keep").unwrap();
        install(&path, b"new executable").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new executable");
        assert_eq!(std::fs::read(&sibling).unwrap(), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn preserves_executable_permissions_and_rejects_linked_targets() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("RisuNest.AppImage");
        std::fs::write(&path, b"installed").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
        install(&path, b"update").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o751);
        let link = root.path().join("linked.AppImage");
        symlink(&path, &link).unwrap();
        assert!(install(&link, b"must not replace").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"update");
    }

    #[cfg(all(unix, feature = "zip"))]
    #[test]
    fn archive_install_uses_the_same_candidate_path_and_requires_a_binary() {
        fn archive(name: &str, data: &[u8]) -> Vec<u8> {
            let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            let mut archive = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            archive.append_data(&mut header, name, data).unwrap();
            archive.into_inner().unwrap().finish().unwrap()
        }
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("RisuNest.AppImage");
        std::fs::write(&path, b"installed").unwrap();
        assert!(install(&path, &archive("other.txt", b"not a binary")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"installed");
        install(&path, &archive("nested/update.AppImage", b"complete binary")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"complete binary");
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    }
}
