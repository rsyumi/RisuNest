use std::path::{Path, PathBuf};

use crate::{Error, Result};

pub(crate) fn validate(bundle: &Path) -> Result<()> {
    if bundle.extension() != Some(std::ffi::OsStr::new("app"))
        || !std::fs::symlink_metadata(bundle)?.is_dir()
        || !std::fs::symlink_metadata(bundle.join("Contents"))?.is_dir()
        || !std::fs::symlink_metadata(bundle.join("Contents/MacOS"))?.is_dir()
        || !std::fs::symlink_metadata(bundle.join("Contents/Info.plist"))?.is_file()
    {
        return Err(Error::FailedToDetermineExtractPath);
    }
    Ok(())
}

pub(crate) fn from_executable(executable: &Path) -> Result<PathBuf> {
    let executable = executable.canonicalize()?;
    let macos = executable.parent().ok_or(Error::FailedToDetermineExtractPath)?;
    let contents = macos.parent().ok_or(Error::FailedToDetermineExtractPath)?;
    let bundle = contents.parent().ok_or(Error::FailedToDetermineExtractPath)?;
    if !executable.is_file()
        || macos.file_name() != Some(std::ffi::OsStr::new("MacOS"))
        || contents.file_name() != Some(std::ffi::OsStr::new("Contents"))
    {
        return Err(Error::FailedToDetermineExtractPath);
    }
    validate(bundle)?;
    Ok(bundle.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_only_an_executable_inside_a_complete_bundle_shape() {
        let root = tempfile::tempdir().unwrap();
        let bundle = root.path().join("RisuNest.app");
        let executable = bundle.join("Contents/MacOS/risunest");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, b"synthetic executable").unwrap();
        assert!(from_executable(&executable).is_err());
        std::fs::write(bundle.join("Contents/Info.plist"), b"synthetic metadata").unwrap();
        assert_eq!(from_executable(&executable).unwrap(), bundle.canonicalize().unwrap());
        assert!(from_executable(executable.parent().unwrap()).is_err());
    }

    #[test]
    fn rejects_a_bare_executable_and_misleading_directory_names() {
        let root = tempfile::tempdir().unwrap();
        for path in [
            "target/debug/risunest",
            "NotAnApp/Contents/MacOS/risunest",
            "RisuNest.app/Contents/MacOS-other/risunest",
            "RisuNest.app/OtherContents/MacOS/risunest",
        ] {
            let executable = root.path().join(path);
            std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
            std::fs::write(&executable, b"synthetic executable").unwrap();
            assert!(from_executable(&executable).is_err(), "{path}");
        }
    }
}
