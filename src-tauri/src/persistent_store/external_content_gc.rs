//! Reclaims external capture content that no durable owner names. A pass
//! recomputes its owners every time, so an interrupted or capped pass leaves
//! only garbage the next one removes.
use super::{PersistentStore, StoreError, StoreResult};
use crate::external_storage::{
    capture::{registered_capture_roots, registered_references, DurableCaptureReference},
    content_store::ContentStore,
};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

/// Removals one pass makes before it stops. What it leaves is collected by
/// the next pass.
const PASS_REMOVALS: usize = 4_096;
const PAGE: usize = 1_024;
/// A capture directory is created before its capture defers collection, so an
/// unregistered one is only abandoned once it is this old.
const ABANDONED_CAPTURE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ContentCollection {
    pub database_bodies: usize,
    pub file_bodies: usize,
    pub capture_directories: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CollectionOutcome {
    /// A capture was writing, an owner could not be read, or the owners
    /// changed while they were being read. Nothing was removed.
    Deferred,
    Collected(ContentCollection),
}

/// Everything that names a body in the app-wide store.
#[derive(PartialEq, Eq)]
struct Owners {
    captures: Vec<DurableCaptureReference>,
}

/// The bodies and catalogs the owners name.
struct Marked {
    database: BTreeSet<String>,
    files: BTreeSet<String>,
    catalogs: BTreeSet<PathBuf>,
}

impl PersistentStore {
    /// One mark/sweep pass over the external capture content store. A capture
    /// takes its collection deferral under the repository mutation lock and
    /// holds it until its bodies are registered, so the lock with no deferral
    /// live means every body a capture has written is named by an owner.
    pub(crate) fn collect_external_content(&mut self) -> StoreResult<CollectionOutcome> {
        let owners = {
            let _repository = crate::asset_repository::coordinator::lock_repository_mutation()?;
            if self.active_readers.asset_inventory_deferred() {
                return Ok(CollectionOutcome::Deferred);
            }
            match self.content_owners() {
                Ok(owners) => owners,
                Err(error) => return deferred(error),
            }
        };
        let marked = match mark(&owners, &self.repository_root) {
            Ok(marked) => marked,
            Err(error) => return deferred(error),
        };
        let _repository = crate::asset_repository::coordinator::lock_repository_mutation()?;
        if self.active_readers.asset_inventory_deferred()
            || !self.content_owners().is_ok_and(|current| current == owners)
        {
            return Ok(CollectionOutcome::Deferred);
        }
        let root = self.repository_root.join("external-storage");
        let mut collection = ContentCollection::default();
        let mut budget = PASS_REMOVALS;
        if let Some(mut content) = ContentStore::open_existing(&root)? {
            collection.database_bodies = sweep_database(&mut content, &marked.database, budget)?;
            budget -= collection.database_bodies;
            collection.file_bodies = content.remove_unnamed_files(&marked.files, budget)?;
            budget -= collection.file_bodies;
        }
        collection.capture_directories =
            sweep_captures(&root.join("captures"), &marked.catalogs, budget)?;
        Ok(CollectionOutcome::Collected(collection))
    }

    /// Runs a pass after a cleanup released bodies. Collection never fails the
    /// operation that released them; a pass that cannot run leaves the bodies
    /// for the next one.
    pub(crate) fn collect_released_external_content(&mut self) {
        if let Err(error) = self.collect_external_content() {
            crate::nlog!("warn", "external content collection failed: {error}");
        }
    }

    /// One bounded step of reclaiming what finished jobs released: the capture
    /// registrations no job holds and no consumer continues from, then, once
    /// none of them is left, one pass. A capture that is writing defers the
    /// whole step, so it changes nothing. True when the step removed
    /// something, so another may find more.
    pub(crate) fn collect_released_captures(&mut self) -> StoreResult<bool> {
        if self.active_readers.asset_inventory_deferred() {
            return Ok(false);
        }
        let released = self.cleanup_capture_cache("")?;
        if released >= super::external_capture::GC_DELETE_LIMIT {
            return Ok(true);
        }
        Ok(match self.collect_external_content()? {
            CollectionOutcome::Deferred => released > 0,
            CollectionOutcome::Collected(collection) => {
                released > 0 || collection != ContentCollection::default()
            }
        })
    }

    fn content_owners(&self) -> StoreResult<Owners> {
        Ok(Owners {
            captures: registered_references(&self.connection, &self.repository_root)?,
        })
    }
}

fn deferred(error: StoreError) -> StoreResult<CollectionOutcome> {
    crate::nlog!("warn", "external content collection deferred: {error}");
    Ok(CollectionOutcome::Deferred)
}

fn mark(owners: &Owners, repository_root: &Path) -> StoreResult<Marked> {
    let roots = registered_capture_roots(
        owners.captures.iter(),
        repository_root,
    )?;
    let files = roots
        .logical_records
        .iter()
        .map(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
                .ok_or_else(|| StoreError::Validation {
                    message: "Registered capture body path is invalid".into(),
                })
        })
        .collect::<StoreResult<_>>()?;
    Ok(Marked {
        database: roots.content_objects,
        files,
        catalogs: roots.catalogs,
    })
}

fn sweep_database(
    content: &mut ContentStore,
    marked: &BTreeSet<String>,
    budget: usize,
) -> StoreResult<usize> {
    let mut unmarked = Vec::new();
    let mut after = String::new();
    while unmarked.len() < budget {
        let page = content.page(&after, PAGE)?;
        let Some((last, _)) = page.last() else {
            break;
        };
        after = last.clone();
        unmarked.extend(
            page.into_iter()
                .map(|(digest, _)| digest)
                .filter(|digest| !marked.contains(digest)),
        );
    }
    unmarked.truncate(budget);
    let mut removed = 0;
    for batch in unmarked.chunks(PAGE) {
        let batch: Vec<&str> = batch.iter().map(String::as_str).collect();
        removed += content.delete(&batch)?;
    }
    Ok(removed)
}

fn sweep_captures(
    captures: &Path,
    marked: &BTreeSet<PathBuf>,
    budget: usize,
) -> StoreResult<usize> {
    let entries = match fs::read_dir(captures) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
    };
    let now = SystemTime::now();
    let mut removed = 0;
    for entry in entries {
        if removed == budget {
            break;
        }
        let directory = entry?.path();
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.is_dir() || crate::trust_boundary::is_link_like(&metadata) {
            continue;
        }
        let catalog = directory.join("capture.sqlite");
        if catalog
            .canonicalize()
            .is_ok_and(|catalog| marked.contains(&catalog))
        {
            continue;
        }
        // The catalog is written for as long as its capture runs; a directory
        // without one is dated by itself.
        let young = fs::symlink_metadata(&catalog)
            .unwrap_or(metadata)
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_none_or(|age| age < ABANDONED_CAPTURE_AGE);
        if young {
            continue;
        }
        for name in ["capture.sqlite", "capture.sqlite-wal", "capture.sqlite-shm"] {
            let path = directory.join(name);
            match fs::symlink_metadata(&path) {
                Ok(file) if file.is_file() && !crate::trust_boundary::is_link_like(&file) => {
                    fs::remove_file(&path)?;
                }
                _ => {}
            }
        }
        // Anything else in it was not put there by a capture; leave it.
        if fs::remove_dir(&directory).is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        crate::trust_boundary::sync_directory(captures)?;
    }
    Ok(removed)
}
