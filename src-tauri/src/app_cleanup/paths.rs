use super::files::{self, failed, Result};
use crate::app_paths::{AppPaths, DataRoot};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub(super) struct Paths {
    /// The data root's entry path, which device secrets are keyed by.
    pub data: PathBuf,
    pub roots: Vec<PathBuf>,
    /// A data root whose entry is a link: it is cleared in place instead of
    /// being one of `roots`.
    pub data_link: Option<DataLink>,
    pub control: PathBuf,
    /// The root that overlaps the installation directory, when one does.
    /// Deleting it can never succeed, so the operation refuses instead.
    pub install_conflict: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub(super) struct DataLink {
    pub link: PathBuf,
    /// `None` when the link's target is missing.
    pub target: Option<PathBuf>,
}

impl Paths {
    pub fn validate(&self) -> Result<()> {
        files::validate(&self.control)?;
        for path in &self.roots {
            files::validate(path)?;
            if self.control.starts_with(path) || path.starts_with(&self.control) {
                return Err("cleanup-path-overlap".into());
            }
        }
        if let Some(link) = &self.data_link {
            files::validate_link(&link.link)?;
            if let Some(target) = &link.target { files::validate_target(target)?; }
        }
        Ok(())
    }

    /// Runs before the operation mutates anything, including before the OS
    /// secret entries are removed. A configuration that cannot complete is
    /// reported as a fault rather than attempted and half applied.
    pub fn preflight(&self) -> Result<()> {
        self.validate()?;
        if self.install_conflict.is_some() {
            return Err("cleanup-root-overlaps-install".into());
        }
        Ok(())
    }

    /// The directory the data root's files are in, when it exists.
    #[cfg(desktop)]
    pub fn data_directory(&self) -> Option<&std::path::Path> {
        match &self.data_link {
            Some(link) => link.target.as_deref(),
            None => Some(&self.data),
        }
    }

    pub fn request(&self) -> PathBuf {
        self.control.join("request.json")
    }

    pub fn from_app(app: &tauri::AppHandle) -> Result<Self> {
        let manifest =
            crate::app_paths::manifest(app).map_err(|error| failed("cleanup-path-unavailable", error))?;
        Ok(Self::from_manifest(&manifest))
    }

    pub fn from_manifest(paths: &AppPaths) -> Self {
        let data_link = match crate::app_paths::inspect_data_root(&paths.data_entry) {
            Ok(DataRoot::Linked(target)) => Some(DataLink { link: paths.data_entry.clone(), target: Some(target) }),
            Ok(DataRoot::Dangling) => Some(DataLink { link: paths.data_entry.clone(), target: None }),
            _ => None,
        };
        let mut owned = paths.owned_roots();
        if data_link.is_some() {
            owned.retain(|root| root != &paths.data && root != &paths.data_entry);
        }
        Self {
            data: paths.data_entry.clone(),
            roots: files::independent_roots(owned),
            data_link,
            control: paths.cleanup_control.clone(),
            install_conflict: paths.install_conflict(),
        }
    }
}
