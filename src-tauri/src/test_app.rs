//! Tauri mock apps for tests.

use crate::persistent_store::commands::PersistentStoreState;
use tauri::{test::MockRuntime, App, Manager};

/// A mock app that closes the databases it manages when it drops. Tauri's path
/// resolver keeps an app handle in the app's own managed state, so a dropped
/// app never drops that state, and a database left in it would keep its files
/// open for the rest of the test process.
pub(crate) struct MockApp(App<MockRuntime>);

pub(crate) fn mock_app() -> MockApp {
    MockApp::new(tauri::test::mock_builder().build(tauri::test::mock_context(tauri::test::noop_assets())).unwrap())
}

impl MockApp {
    pub(crate) fn new(app: App<MockRuntime>) -> Self { Self(app) }
}

impl std::ops::Deref for MockApp {
    type Target = App<MockRuntime>;
    fn deref(&self) -> &Self::Target { &self.0 }
}

impl Drop for MockApp {
    fn drop(&mut self) {
        if let Some(state) = self.0.try_state::<PersistentStoreState>() {
            state.close_for_test();
        }
        if let Some(state) = self.0.try_state::<crate::device_backup::DeviceBackupState>() {
            let _ = state.close_for_cleanup();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistent_store::PersistentStore;
    use std::path::{Path, PathBuf};

    fn wal_files(root: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_owned()];
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.to_string_lossy().ends_with("-wal") {
                    found.push(path);
                }
            }
        }
        found
    }

    #[test]
    fn dropping_the_app_closes_the_store_it_manages() {
        let directory = tempfile::tempdir().unwrap();
        let app = mock_app();
        app.manage(PersistentStoreState::with_test_store(PersistentStore::open(directory.path()).unwrap()));
        assert!(!wal_files(directory.path()).is_empty());
        drop(app);
        // Closing a database's last connection removes its WAL file.
        assert_eq!(wal_files(directory.path()), Vec::<PathBuf>::new());
    }
}
