//! Shrinks the store files when the user asks. SQLite reuses the pages that
//! deletions free but never returns them to the file system, so the start after
//! a request copies each file without its free pages and puts the copy in its
//! place before the store opens.

use super::{device_store::DEVICE_DATABASE_FILE, PersistentStore, StoreError, StoreResult};
use rusqlite::Connection;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// In `persistent/`: the next start compacts the store files.
const REQUEST_FILE: &str = "store-compaction-requested";
/// In `persistent/`: the start notice offered compaction since its condition last held.
const OFFERED_FILE: &str = "store-compaction-offered";
/// Free space a copy leaves for the other writes to its volume.
const SPACE_MARGIN_BYTES: u64 = 64 << 20;
/// The start notice offers compaction from this many free bytes, when they are
/// also at least the used bytes.
const OFFER_FREE_BYTES: u64 = 1 << 30;
/// Virtual machine steps between two looks at the clock while a file copies.
const PROGRESS_STEPS: std::os::raw::c_int = 1_000;
/// How often a copy reports its length.
pub(crate) const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// The pages of one store file, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pages {
    pub(crate) used_bytes: u64,
    pub(crate) free_bytes: u64,
}

impl Pages {
    /// Reads header values only, so this costs the same however large the file is.
    fn read(connection: &Connection) -> StoreResult<Self> {
        let pragma = |name: &str| -> StoreResult<u64> {
            let value: i64 = connection.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))?;
            u64::try_from(value).map_err(|_| StoreError::Store { message: format!("{name} is negative") })
        };
        let (page_size, page_count, free_count) = (pragma("page_size")?, pragma("page_count")?, pragma("freelist_count")?);
        Ok(Self {
            used_bytes: page_count.saturating_sub(free_count).saturating_mul(page_size),
            free_bytes: free_count.saturating_mul(page_size),
        })
    }
}

/// How a compacting start ended.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub(crate) enum StartCompaction {
    Compacted { reclaimed_bytes: u64 },
    /// The volume lacked this many bytes for a file.
    NoSpace { shortfall_bytes: u64 },
    Failed,
}

/// Bytes a compacting start copied so far, out of the bytes it copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CompactionProgress {
    pub(crate) done_bytes: u64,
    pub(crate) total_bytes: u64,
}

/// How copying one file ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FileCompaction {
    /// The copy took the file's place; file lengths before and after.
    Replaced { before_bytes: u64, after_bytes: u64 },
    /// The volume lacked this many bytes, so nothing was copied.
    NoSpace { shortfall_bytes: u64 },
    /// Another connection still had the file open, so the copy was discarded.
    InUse,
}

impl PersistentStore {
    /// Each store file the store has open, with its pages.
    pub(crate) fn database_pages(&self) -> StoreResult<Vec<(PathBuf, Pages)>> {
        let mut files = vec![(self.database_path.clone(), Pages::read(&self.connection)?)];
        if let Ok(device) = &self.device_store {
            files.push((self.database_path.with_file_name(DEVICE_DATABASE_FILE), Pages::read(device.connection())?));
        }
        Ok(files)
    }

    /// Records a compaction for the next start. Returns the bytes the volume
    /// lacks for the largest copy instead, and records nothing then.
    pub(crate) fn request_compaction(&self, available_space: impl FnOnce(&Path) -> io::Result<u64>) -> StoreResult<Option<u64>> {
        let needed = self.database_pages()?.iter()
            .filter(|(_, pages)| pages.free_bytes > 0)
            .map(|(_, pages)| space_needed(pages.used_bytes))
            .max()
            .unwrap_or(0);
        let persistent = self.repository_root.join("persistent");
        let available = available_space(&persistent)?;
        if available < needed {
            return Ok(Some(needed - available));
        }
        write_durably(&persistent.join(REQUEST_FILE))?;
        Ok(None)
    }

    /// The reclaimable bytes the start notice names when it offers compaction
    /// now. It offers once while the condition holds.
    pub(crate) fn compaction_offer(&self) -> StoreResult<Option<u64>> {
        self.compaction_offer_from(OFFER_FREE_BYTES)
    }

    fn compaction_offer_from(&self, min_free_bytes: u64) -> StoreResult<Option<u64>> {
        let files = self.database_pages()?;
        let free = files.iter().map(|(_, pages)| pages.free_bytes).sum::<u64>();
        let used = files.iter().map(|(_, pages)| pages.used_bytes).sum::<u64>();
        let offered = self.repository_root.join("persistent").join(OFFERED_FILE);
        if free < min_free_bytes || free < used {
            remove_if_present(&offered)?;
            return Ok(None);
        }
        if offered.try_exists()? {
            return Ok(None);
        }
        fs::write(&offered, [])?;
        Ok(Some(free))
    }
}

pub(crate) fn requested(app_data_dir: &Path) -> io::Result<bool> {
    request_path(app_data_dir).try_exists()
}

/// Compacts each of the closed store `files` that has free pages and removes
/// the request. `exclusive` says whether the caller keeps every other writer
/// of the files out meanwhile; without it nothing is copied. `report` receives
/// the bytes copied so far.
pub(crate) fn compact_requested(
    app_data_dir: &Path,
    files: Vec<(PathBuf, Pages)>,
    exclusive: bool,
    available_space: impl Fn(&Path) -> io::Result<u64>,
    interval: Duration,
    report: impl Fn(CompactionProgress) + Clone + Send + 'static,
) -> StartCompaction {
    // Removed before any copy, so a start the user interrupts is not repeated at later starts.
    if let Err(error) = remove_if_present(&request_path(app_data_dir)) {
        crate::nlog!("warn", "Store compaction request was not removed: {error}");
        return StartCompaction::Failed;
    }
    if !exclusive {
        crate::nlog!("warn", "Store compaction was skipped: another task holds the store files");
        return StartCompaction::Failed;
    }
    let files: Vec<_> = files.into_iter().filter(|(_, pages)| pages.free_bytes > 0).collect();
    let total_bytes = files.iter().map(|(_, pages)| pages.used_bytes).sum();
    let (mut done_bytes, mut reclaimed_bytes, mut shortfall_bytes, mut failed) = (0, 0, 0, false);
    for (path, pages) in files {
        let copied = {
            let report = report.clone();
            move |bytes: u64| report(CompactionProgress { done_bytes: done_bytes + bytes.min(pages.used_bytes), total_bytes })
        };
        match compact_closed_file(&path, app_data_dir, &available_space, interval, copied) {
            Ok(FileCompaction::Replaced { before_bytes, after_bytes }) => {
                reclaimed_bytes += before_bytes.saturating_sub(after_bytes);
            }
            Ok(FileCompaction::NoSpace { shortfall_bytes: lacking }) => {
                crate::nlog!("warn", "Store compaction lacked {lacking} bytes of free space");
                shortfall_bytes = shortfall_bytes.max(lacking);
            }
            Ok(FileCompaction::InUse) => {
                crate::nlog!("warn", "Store compaction left a file another connection holds");
                failed = true;
            }
            Err(error) => {
                crate::nlog!("warn", "Store compaction failed: {error}");
                failed = true;
            }
        }
        done_bytes += pages.used_bytes;
        report(CompactionProgress { done_bytes, total_bytes });
    }
    if failed {
        StartCompaction::Failed
    } else if shortfall_bytes > 0 {
        StartCompaction::NoSpace { shortfall_bytes }
    } else {
        StartCompaction::Compacted { reclaimed_bytes }
    }
}

/// Copies the closed store file at `path` without its free pages into the
/// managed scratch directory and puts the copy in its place. Content, rowids
/// and the header values stay the same; the journal mode is the one the
/// store's open sets. `report` receives the length of the copy so far, at most
/// once per `interval` while it is written.
pub(crate) fn compact_closed_file(
    path: &Path,
    repository_root: &Path,
    available_space: impl FnOnce(&Path) -> io::Result<u64>,
    interval: Duration,
    mut report: impl FnMut(u64) + Send + 'static,
) -> StoreResult<FileCompaction> {
    let connection = crate::sqlite_open::open(path)?;
    connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    let pages = Pages::read(&connection)?;
    let before_bytes = fs::metadata(path)?.len();
    let scratch = crate::external_storage::leftovers::managed_scratch(repository_root, "store-compaction-")
        .map_err(|error| StoreError::Store { message: error.to_string() })?;
    let (needed, available) = (space_needed(pages.used_bytes), available_space(scratch.path())?);
    if available < needed {
        return Ok(FileCompaction::NoSpace { shortfall_bytes: needed - available });
    }
    let copy = scratch.path().join("compacted.sqlite");
    let name = crate::sqlite_open::sqlite_path(&copy).to_str()
        .ok_or_else(|| StoreError::Store { message: "compaction path is not valid UTF-8".to_owned() })?
        .to_owned();
    let watched = copy.clone();
    let mut reported = Instant::now();
    connection.progress_handler(PROGRESS_STEPS, Some(move || {
        if reported.elapsed() >= interval {
            reported = Instant::now();
            if let Ok(metadata) = fs::metadata(&watched) {
                report(metadata.len());
            }
        }
        false
    }))?;
    connection.execute("VACUUM INTO ?1", [name])?;
    connection.close().map_err(|(_, error)| error)?;
    // SQLite removes the WAL when the last connection to a file closes, so one
    // still there belongs to a connection that could commit to the old file.
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    if Path::new(&wal).try_exists()? {
        return Ok(FileCompaction::InUse);
    }
    OpenOptions::new().write(true).open(&copy)?.sync_all()?;
    super::export::destination::atomic_replace(&copy, path)?;
    if let Some(directory) = path.parent() {
        crate::trust_boundary::sync_directory(directory)?;
    }
    Ok(FileCompaction::Replaced { before_bytes, after_bytes: fs::metadata(path)?.len() })
}

fn space_needed(used_bytes: u64) -> u64 {
    used_bytes.saturating_add(SPACE_MARGIN_BYTES)
}

fn request_path(app_data_dir: &Path) -> PathBuf {
    app_data_dir.join("persistent").join(REQUEST_FILE)
}

fn write_durably(path: &Path) -> io::Result<()> {
    fs::File::create(path)?.sync_all()?;
    sync_parent(path)
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => sync_parent(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn sync_parent(path: &Path) -> io::Result<()> {
    if let Some(directory) = path.parent() {
        crate::trust_boundary::sync_directory(directory)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// About 4 MiB in use, more than the default page cache, so the copy grows
    /// while it is written, and about 16 MiB free.
    fn database(root: &Path) -> PathBuf {
        let path = root.join("store.sqlite");
        let connection = crate::sqlite_open::open(&path).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA user_version=7; PRAGMA application_id=1234;
            CREATE TABLE kept(value TEXT NOT NULL);
            CREATE TABLE bodies(hash TEXT PRIMARY KEY, body BLOB NOT NULL);
            CREATE INDEX body_sizes ON bodies(length(body));").unwrap();
        let tx = connection.unchecked_transaction().unwrap();
        for index in 0..2_000 {
            tx.execute("INSERT INTO bodies VALUES(?1,?2)", rusqlite::params![format!("synthetic-{index}"), vec![7u8; 8_192]]).unwrap();
        }
        for index in 0..30_000 {
            tx.execute("INSERT INTO kept VALUES(?1)", [format!("synthetic-kept-{index:0>120}")]).unwrap();
        }
        tx.execute("DELETE FROM kept WHERE rowid % 3 = 0", []).unwrap();
        tx.execute("DELETE FROM bodies WHERE hash<>'synthetic-7'", []).unwrap();
        tx.commit().unwrap();
        path
    }

    /// `kept` has no index, so it shows whether the copy renumbered rowids, as a
    /// plain `VACUUM` does for such a table.
    fn snapshot(path: &Path) -> (Vec<(i64, String)>, Vec<String>, i64, i64, String) {
        let connection = crate::sqlite_open::open(path).unwrap();
        // As the store's open does.
        connection.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        let rows = connection.prepare("SELECT rowid,value FROM kept ORDER BY rowid").unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
        let schema = connection.prepare("SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name").unwrap()
            .query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        let read = |sql: &str| connection.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
        (rows, schema, read("PRAGMA user_version"), read("PRAGMA application_id"),
            connection.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap())
    }

    fn pages(path: &Path) -> Pages {
        Pages::read(&crate::sqlite_open::open(path).unwrap()).unwrap()
    }

    fn scratch_entries(root: &Path) -> usize {
        std::fs::read_dir(root.join("external-storage").join("scratch")).map_or(0, |entries| entries.count())
    }

    fn recorder() -> (Arc<Mutex<Vec<u64>>>, impl FnMut(u64) + Send + 'static) {
        let reports = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&reports);
        (reports, move |bytes| sink.lock().unwrap().push(bytes))
    }

    #[test]
    fn a_closed_sparse_file_is_replaced_by_a_copy_with_its_rows_rowids_and_header() {
        let root = tempfile::tempdir().unwrap();
        let path = database(root.path());
        let before = snapshot(&path);
        let grown = pages(&path);
        let (reports, report) = recorder();
        let FileCompaction::Replaced { before_bytes, after_bytes } =
            compact_closed_file(&path, root.path(), |_| Ok(u64::MAX), Duration::ZERO, report).unwrap()
        else { panic!("the file was not replaced") };
        assert_eq!(snapshot(&path), before);
        assert_eq!(pages(&path), Pages { used_bytes: after_bytes, free_bytes: 0 });
        assert!(after_bytes <= grown.used_bytes && after_bytes < before_bytes / 3, "{after_bytes} of {before_bytes}");
        let reports = reports.lock().unwrap().clone();
        assert!(reports.windows(2).all(|pair| pair[0] <= pair[1]) && reports.iter().all(|&bytes| bytes <= after_bytes), "{reports:?}");
        assert!(reports.iter().any(|&bytes| bytes > 0 && bytes < after_bytes), "the copy reported no progress while written: {reports:?}");
        let reopened = crate::sqlite_open::open(&path).unwrap();
        assert_eq!(reopened.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)).unwrap(), "ok");
        reopened.execute("INSERT INTO kept VALUES('synthetic-after')", []).unwrap();
        assert_eq!(scratch_entries(root.path()), 0, "the scratch copy was left behind");
    }

    #[test]
    fn a_file_another_connection_holds_or_without_room_for_its_copy_is_left_as_it_is() {
        let root = tempfile::tempdir().unwrap();
        let path = database(root.path());
        let before = snapshot(&path);
        let grown = pages(&path);
        let holder = crate::sqlite_open::open(&path).unwrap();
        holder.query_row("SELECT count(*) FROM kept", [], |_| Ok(())).unwrap();
        let (_, report) = recorder();
        assert_eq!(compact_closed_file(&path, root.path(), |_| Ok(u64::MAX), Duration::ZERO, report).unwrap(), FileCompaction::InUse);
        drop(holder);
        assert_eq!(pages(&path), grown);
        assert_eq!(scratch_entries(root.path()), 0);

        let (_, report) = recorder();
        let available = grown.used_bytes;
        assert_eq!(
            compact_closed_file(&path, root.path(), |_| Ok(available), Duration::ZERO, report).unwrap(),
            FileCompaction::NoSpace { shortfall_bytes: SPACE_MARGIN_BYTES },
        );
        let (_, report) = recorder();
        assert!(compact_closed_file(&path, root.path(), |_| Err(io::Error::other("synthetic")), Duration::ZERO, report).is_err());
        assert_eq!(pages(&path), grown);
        assert_eq!(snapshot(&path), before);
        assert_eq!(scratch_entries(root.path()), 0);
    }

    #[test]
    fn the_start_notice_offers_once_until_a_check_finds_too_little_to_reclaim() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(root.path()).unwrap();
        let offered = root.path().join("persistent").join(OFFERED_FILE);
        assert_eq!(store.compaction_offer_from(1 << 20).unwrap(), None);
        store.connection.execute_batch("CREATE TABLE synthetic_filler(body BLOB NOT NULL);
            INSERT INTO synthetic_filler WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i+1 FROM n WHERE i<1024)
                SELECT randomblob(4096) FROM n;
            DROP TABLE synthetic_filler;").unwrap();
        let free = store.database_pages().unwrap().iter().map(|(_, pages)| pages.free_bytes).sum::<u64>();
        assert_eq!(store.compaction_offer_from(1 << 20).unwrap(), Some(free));
        assert_eq!(store.compaction_offer_from(1 << 20).unwrap(), None, "the notice offered twice");
        assert_eq!(store.compaction_offer_from(free + 1).unwrap(), None);
        assert!(!offered.exists());
        assert_eq!(store.compaction_offer_from(1 << 20).unwrap(), Some(free));
    }
}
