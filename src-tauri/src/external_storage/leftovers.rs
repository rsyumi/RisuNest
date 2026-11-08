//! Directories nothing can own again: what a crashed publication was still
//! building, what a finished restore downloaded, and what a removed connection
//! kept. Runs at startup, before any job can claim a connection.
use super::{
    connection_store::ConnectionStore,
    contract::Result,
    job_store::{JobCommandState, JobStore},
    runtime::{connection_directory, job_directory, local_error},
    runtime_restore::discard_finished_staging,
};
use crate::trust_boundary::is_link_like;
use tauri::{AppHandle, Manager, Runtime};
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

fn connection_directories(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let entries = match fs::read_dir(root.join("external-storage")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        other => other?,
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.len() == 64 && name.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) {
            found.push((name, entry.path()));
        }
    }
    Ok(found)
}

fn linked() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "external storage path is redirected")
}

/// Every directory from the external storage root down to `directory` must
/// be a real directory. `false` when it was never created.
pub(super) fn managed_directory(root: &Path, directory: &Path) -> io::Result<bool> {
    let base = root.join("external-storage");
    if !directory.starts_with(&base) {
        return Err(linked());
    }
    let chain: Vec<PathBuf> = directory
        .ancestors()
        .take_while(|path| path.starts_with(&base))
        .map(Path::to_path_buf)
        .collect();
    for path in chain.iter().rev() {
        match fs::symlink_metadata(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
            Ok(metadata) if is_link_like(&metadata) || !metadata.is_dir() => return Err(linked()),
            Ok(_) => {}
        }
    }
    Ok(true)
}

/// Deletes without ever following a link; a link stops the removal instead.
pub(super) fn remove_tree(path: &Path) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        other => other?,
    };
    if is_link_like(&metadata) {
        return Err(linked());
    }
    let removed = if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            remove_tree(&entry?.path())?;
        }
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    };
    match removed {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn remove_managed(root: &Path, directory: &Path) -> io::Result<()> {
    if managed_directory(root, directory)? {
        remove_tree(directory)?;
    }
    Ok(())
}

/// The app-owned directory that holds every scratch directory. Startup empties
/// it, so whatever a crash left there is gone by the next run.
pub(crate) fn scratch_directory(root: &Path) -> io::Result<PathBuf> {
    let directory = root.join("external-storage").join("scratch");
    fs::create_dir_all(&directory)?;
    if !managed_directory(root, &directory)? {
        return Err(io::Error::new(io::ErrorKind::NotFound, "scratch directory is unavailable"));
    }
    Ok(directory)
}

pub(crate) fn local_scratch(root: &Path, prefix: &str) -> io::Result<tempfile::TempDir> {
    tempfile::Builder::new().prefix(prefix).tempdir_in(scratch_directory(root)?)
}

pub(crate) fn managed_scratch(root: &Path, prefix: &str) -> Result<tempfile::TempDir> {
    local_scratch(root, prefix).map_err(local_error)
}

pub(crate) fn remove_connection_directory(root: &Path, connection_id: &str) -> io::Result<()> {
    remove_managed(root, &connection_directory(root, connection_id))
}

pub(crate) fn remove_at_startup(root: &Path) {
    if !root.join("external-storage").is_dir() {
        return;
    }
    if let Err(error) = remove_unowned(root) {
        crate::nlog!("warn", "External storage leftovers were not removed: {error}");
    }
}

/// How long a collection step waits for native file admission another
/// operation holds.
const COLLECTION_BUSY_RETRY: Duration = Duration::from_secs(30);

/// Collects what earlier runs released and left: capture registrations no job
/// holds and the bodies and directories only they named. Runs once the
/// renderer has opened the store, a bounded step at a time, each under native
/// file admission so no capture or replacement runs meanwhile.
pub(crate) fn collect_released_content_later<R: Runtime>(app: &AppHandle<R>) {
    let (Some(_), Some(files)) = (
        app.try_state::<JobCommandState>(),
        app.try_state::<crate::native_file_jobs::NativeFileJobState>(),
    ) else {
        return;
    };
    let admission = files.admission.clone();
    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("external-content-collection".into())
        .spawn(move || loop {
            let Some(step) = app.state::<JobCommandState>().background_step() else {
                return;
            };
            let Ok(permit) = admission.file(false) else {
                drop(step);
                std::thread::sleep(COLLECTION_BUSY_RETRY);
                continue;
            };
            let collected = super::runtime::native_store(&app)
                .and_then(|mut pds| pds.collect_released_captures().map_err(local_error));
            drop(permit);
            match collected {
                Ok(true) => {}
                Ok(false) => return,
                Err(error) => {
                    crate::nlog!("warn", "Released external content was not collected: {error}");
                    return;
                }
            }
        });
    if let Err(error) = spawned {
        crate::nlog!("warn", "Released external content collection did not start: {error}");
    }
}

/// Runs only after the normal PDS open has passed startup recovery admission.
/// Each pass visits one page, including owners that were retained last time.
pub(crate) fn reclaim_terminal_spools_later(app: &AppHandle) {
    let Ok(root) = super::runtime::root(app) else { return; };
    if !root.join("external-jobs.sqlite").is_file() { return; }
    let worker = app.state::<JobCommandState>().track_worker();
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _worker = worker;
        let result = (|| -> Result<()> {
            let jobs = JobStore::open(&root)?;
            let page = jobs.terminal_spool_page()?;
            let admission = app.state::<crate::native_file_jobs::NativeFileJobState>()
                .admission.clone();
            for (_, job) in &page {
                let Ok((cancel, _claim)) = app.state::<JobCommandState>().claim(job) else { continue; };
                let result = (|| -> Result<()> {
                    let _permit = admission.file(false).map_err(local_error)?;
                    cancel.check()?;
                    let mut pds = super::runtime::native_store(&app)?;
                    reclaim_terminal_spool(&root, &jobs, &mut pds, &job.id)
                })();
                if let Err(error) = result {
                    crate::nlog!("warn", "External terminal spool was retained: {error}");
                }
            }
            jobs.advance_terminal_spool_page(&page)?;
            jobs.prune_released()
        })();
        if let Err(error) = result {
            crate::nlog!("warn", "External terminal spools were not examined: {error}");
        }
    });
}

// The caller holds this job's connection claim and native file admission.
fn reclaim_terminal_spool(
    root: &Path,
    jobs: &JobStore,
    pds: &mut crate::persistent_store::PersistentStore,
    id: &str,
) -> Result<()> {
    let job = jobs.read(id)?;
    if !job.terminal() || job.spool_released { return Ok(()); }
    let repository = match pds.external_job(id).map_err(local_error)? {
        Some(owner) => {
            if owner.connection_id != job.request.connection_id {
                return Err(super::contract::ProviderError::new(super::contract::ErrorKind::Corrupt));
            }
            owner.repository_id
        }
        // Without an authoritative job, cleanup can only release bookkeeping
        // when no journal or registered capture owner exists.
        None => String::new(),
    };
    let cleanup = super::journal::TransferJournal::cleanup_terminal_spools_at(
        &job_directory(root, &job.request.connection_id, id), id, pds, &repository,
    )?;
    if matches!(cleanup, super::journal::SpoolCleanup::Removed { .. }) {
        jobs.release_spool(id)?;
    }
    Ok(())
}

fn remove_unowned(root: &Path) -> Result<()> {
    remove_managed(root, &root.join("external-storage").join("scratch")).map_err(local_error)?;
    let directories = connection_directories(root).map_err(local_error)?;
    let live: BTreeSet<String> = ConnectionStore::open(root)?
        .ids()?
        .iter()
        .map(|id| connection_directory(root, id))
        .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
        .collect();
    for (name, directory) in directories {
        let removed = if live.contains(&name) {
            remove_managed(root, &directory.join("package-cache").join("build"))
        } else {
            remove_managed(root, &directory)
        };
        if let Err(error) = removed {
            crate::nlog!("warn", "External storage leftover was not removed: {error}");
        }
    }
    if root.join("external-jobs.sqlite").is_file() {
        let jobs = JobStore::open(root)?;
        let mut after = 0;
        loop {
            let page = jobs.finished_restore_page(after)?;
            if page.is_empty() { break; }
            for (cursor, job) in page {
                discard_finished_staging(root, &job);
                after = cursor;
            }
        }
        jobs.prune_released()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::job_store::{DurableJob, JobKind};
    use crate::external_storage::runtime::job_directory;

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"synthetic").unwrap();
    }

    #[test]
    fn terminal_spool_reconciliation_releases_only_unowned_terminal_jobs() {
        let root = tempfile::tempdir().unwrap();
        let mut pds = crate::persistent_store::PersistentStore::open(root.path()).unwrap();
        let jobs = JobStore::open(root.path()).unwrap();
        let identity = pds.external_identity().unwrap();
        let request = serde_json::from_value(serde_json::json!({
            "connectionId":"removed-connection", "kind":"backup"
        })).unwrap();
        let mut terminal = DurableJob::new(request, 1, identity.clone());
        terminal.summary["state"] = serde_json::json!("failed");
        jobs.put(&terminal).unwrap();
        let unrelated = job_directory(root.path(), "removed-connection", &terminal.id)
            .join("unregistered.partial");
        write(&unrelated);
        reclaim_terminal_spool(root.path(), &jobs, &mut pds, &terminal.id).unwrap();
        assert!(jobs.read(&terminal.id).unwrap().spool_released);
        assert!(unrelated.is_file());
        reclaim_terminal_spool(root.path(), &jobs, &mut pds, &terminal.id).unwrap();

        let mut held = terminal.clone();
        held.id = uuid::Uuid::new_v4().to_string();
        held.spool_released = false;
        jobs.put(&held).unwrap();
        let mut db = rusqlite::Connection::open(root.path().join("persistent/persistent.sqlite")).unwrap();
        let tx = db.transaction().unwrap();
        crate::persistent_store::external_storage_state::register_capture(
            &tx, "capture", &identity, "scope", "logical-v1", "", &"a".repeat(64), "connection",
        ).unwrap();
        tx.commit().unwrap();
        pds.retain_external_capture("capture", &held.id).unwrap();
        reclaim_terminal_spool(root.path(), &jobs, &mut pds, &held.id).unwrap();
        assert!(!jobs.read(&held.id).unwrap().spool_released);

        let mut live = held.clone();
        live.id = uuid::Uuid::new_v4().to_string();
        live.summary["state"] = serde_json::json!("waiting");
        jobs.put(&live).unwrap();
        reclaim_terminal_spool(root.path(), &jobs, &mut pds, &live.id).unwrap();
        assert!(!jobs.read(&live.id).unwrap().spool_released);
    }

    #[test]
    fn startup_removes_a_removed_connection_and_keeps_other_entries() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let removed = connection_directory(root, "removed-connection");
        write(&removed.join("package-cache").join("snapshot-cache.sqlite"));
        write(&removed.join("jobs").join("job").join("transfers.sqlite"));
        let captures = root.join("external-storage").join("captures").join("capture");
        write(&captures.join("capture.sqlite"));
        let other = root.join("external-storage").join("A".repeat(64));
        write(&other.join("kept"));
        remove_at_startup(root);
        assert!(!removed.exists());
        assert!(captures.join("capture.sqlite").is_file());
        assert!(other.join("kept").is_file());
    }

    fn store_connection(root: &Path, table: &str, id: &str) {
        drop(ConnectionStore::open(root).unwrap());
        rusqlite::Connection::open(root.join("external-connections.sqlite"))
            .unwrap()
            .execute(&format!("INSERT INTO {table}(id,value) VALUES(?1,'{{}}')"), [id])
            .unwrap();
    }

    fn restore(root: &Path, connection: &str, state: &str) -> PathBuf {
        let request = serde_json::from_value(serde_json::json!({
            "connectionId": connection, "kind": "restore",
            "snapshotId": "snapshot", "targetRevision": "1"
        }))
        .unwrap();
        let identity = crate::persistent_store::sync_selection::CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "library".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 1,
        };
        let mut job = DurableJob::new(request, 1, identity);
        assert_eq!(job.request.kind, JobKind::Restore);
        job.summary["state"] = serde_json::json!(state);
        JobStore::open(root).unwrap().put(&job).unwrap();
        let staging = job_directory(root, connection, &job.id).join("restore-snapshot");
        write(&staging.join("plaintext").join("record"));
        staging
    }

    #[test]
    fn startup_clears_live_connection_builds_and_finished_restores_only() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        store_connection(root, "connections", "live");
        store_connection(root, "pending_connections", "pending");
        let live = connection_directory(root, "live");
        write(&live.join("package-cache").join("build").join("stale-pack"));
        write(&live.join("package-cache").join("snapshot-cache.sqlite"));
        let pending = connection_directory(root, "pending");
        write(&pending.join("jobs").join("job").join("transfers.sqlite"));
        let failed = restore(root, "live", "failed");
        let cancelled = restore(root, "live", "cancelled");
        let paused = restore(root, "live", "waiting");
        remove_at_startup(root);
        assert!(!live.join("package-cache").join("build").exists());
        assert!(live.join("package-cache").join("snapshot-cache.sqlite").is_file());
        assert!(pending.join("jobs").join("job").join("transfers.sqlite").is_file());
        assert!(!failed.exists());
        assert!(!cancelled.exists());
        assert!(paused.join("plaintext").join("record").is_file());
    }

    #[test]
    fn no_external_storage_means_no_store_is_created() {
        let root = tempfile::tempdir().unwrap();
        remove_at_startup(root.path());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    fn link_directory(target: &Path, link: &Path) {
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[cfg(windows)]
    fn link_directory(target: &Path, link: &Path) {
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new("cmd")
            .creation_flags(0x08000000)
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(output.status.success(), "create junction: {output:?}");
    }

    #[test]
    fn a_removed_connection_directory_is_deleted_but_never_through_a_link() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let directory = connection_directory(root, "connection");
        write(&directory.join("package-cache").join("build").join("pack"));
        remove_connection_directory(root, "connection").unwrap();
        assert!(!directory.exists());
        remove_connection_directory(root, "connection").unwrap();
        let outside = tempfile::tempdir().unwrap();
        write(&outside.path().join("kept"));
        link_directory(outside.path(), &directory);
        assert!(remove_connection_directory(root, "connection").is_err());
        assert!(outside.path().join("kept").is_file());
    }
    #[test]
    fn a_path_outside_or_a_file_inside_external_storage_is_never_managed() {
        let root = tempfile::tempdir().unwrap();
        assert!(managed_directory(root.path(), &root.path().join("outside")).is_err());
        let directory = connection_directory(root.path(), "connection");
        write(&directory);
        assert!(managed_directory(root.path(), &directory).is_err());
        assert!(remove_connection_directory(root.path(), "connection").is_err());
        assert_eq!(fs::read(&directory).unwrap(), b"synthetic");
    }
    #[test]
    fn startup_removes_only_managed_abandoned_scratch() {
        let root = tempfile::tempdir().unwrap();
        let scratch = managed_scratch(root.path(), "export-").unwrap();
        std::fs::write(scratch.path().join("body"), b"synthetic").unwrap();
        let orphan = scratch.keep();
        let unrelated = root.path().join("unrelated");
        std::fs::create_dir(&unrelated).unwrap();
        std::fs::write(unrelated.join("keep"), b"keep").unwrap();
        remove_at_startup(root.path());
        assert!(!orphan.exists());
        assert!(unrelated.join("keep").is_file());
    }

}
