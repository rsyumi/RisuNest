//! Records which tables and indexes fill the store files, once per process, so a
//! report of an unexpectedly large database can be answered from the log.

use rusqlite::{Connection, InterruptHandle};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Condvar, Mutex, MutexGuard, PoisonError},
    time::Duration,
};

static BREAKDOWNS: Breakdowns = Breakdowns::new();
const LISTED: usize = 12;

/// Logs the breakdown of `files` on a thread of its own, unless this process
/// already logged one. Reading every page of a large store takes a while, so
/// it never runs on the caller's thread or under the store lock.
pub(super) fn log_once(files: Vec<PathBuf>) {
    BREAKDOWNS.start(files, breakdown);
}

/// Ends a running breakdown and returns once its connection is closed, so a
/// store closed for maintenance and opened again finds no reader holding the
/// write-ahead log. A stopped breakdown runs again at the next storage stats read.
pub(super) fn stop() {
    drop(BREAKDOWNS.stopped());
}

/// Runs `scan` in place of the breakdown, whether or not one already ran, for
/// tests that need a read lasting until it is stopped.
#[cfg(test)]
pub(super) fn start_for_test(
    files: Vec<PathBuf>,
    scan: impl Fn(&Connection, &Path) -> rusqlite::Result<String> + Send + 'static,
) {
    *BREAKDOWNS.stopped() = Scan { started: true, running: true, ..Scan::default() };
    BREAKDOWNS.spawn(files, scan);
}

struct Breakdowns {
    state: Mutex<Scan>,
    finished: Condvar,
}

#[derive(Default)]
struct Scan {
    started: bool,
    running: bool,
    stopped: bool,
    interrupt: Option<InterruptHandle>,
}

impl Breakdowns {
    const fn new() -> Self {
        Self {
            state: Mutex::new(Scan { started: false, running: false, stopped: false, interrupt: None }),
            finished: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Scan> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn start(
        &'static self,
        files: Vec<PathBuf>,
        scan: impl Fn(&Connection, &Path) -> rusqlite::Result<String> + Send + 'static,
    ) {
        {
            let mut state = self.lock();
            if state.started || state.running {
                return;
            }
            *state = Scan { started: true, running: true, ..Scan::default() };
        }
        self.spawn(files, scan);
    }

    /// Runs `scan` over `files` for a start that already marked the scan running.
    fn spawn(
        &'static self,
        files: Vec<PathBuf>,
        scan: impl Fn(&Connection, &Path) -> rusqlite::Result<String> + Send + 'static,
    ) {
        let spawned = std::thread::Builder::new()
            .name("pds-storage-breakdown".to_owned())
            .spawn(move || {
                let _finished = Finished(self);
                for path in files {
                    let connection = match open(&path) {
                        Ok(connection) => connection,
                        Err(error) => {
                            crate::nlog!("warn", "storage breakdown of {} failed: {error}", name(&path));
                            continue;
                        }
                    };
                    {
                        let mut state = self.lock();
                        if state.stopped {
                            break;
                        }
                        state.interrupt = Some(connection.get_interrupt_handle());
                    }
                    let line = scan(&connection, &path);
                    {
                        let mut state = self.lock();
                        state.interrupt = None;
                        if state.stopped {
                            break;
                        }
                    }
                    match line {
                        Ok(line) => crate::nlog!("info", "{line}"),
                        Err(error) => crate::nlog!("warn", "storage breakdown of {} failed: {error}", name(&path)),
                    }
                }
            });
        if spawned.is_err() {
            *self.lock() = Scan::default();
            self.finished.notify_all();
        }
    }

    /// Stops the scan, waits for its thread to close the connection and returns the lock.
    fn stopped(&self) -> MutexGuard<'_, Scan> {
        let mut state = self.lock();
        state.stopped = true;
        while state.running {
            // An interrupt that arrives between two statements is lost, so it repeats until the scan ends.
            if let Some(interrupt) = &state.interrupt {
                interrupt.interrupt();
            }
            state = self
                .finished
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        state
    }
}

/// Marks the scan finished once its thread has closed the connection, however the thread ends.
struct Finished(&'static Breakdowns);

impl Drop for Finished {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.running = false;
        state.interrupt = None;
        if state.stopped {
            state.started = false;
        }
        drop(state);
        self.0.finished.notify_all();
    }
}

/// One line naming the file's size, its free pages and its largest b-trees.
fn breakdown(connection: &Connection, path: &Path) -> rusqlite::Result<String> {
    let pragma = |name: &str| -> rusqlite::Result<u64> {
        connection.query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0)).map(nonnegative)
    };
    let (page_size, pages, free) = (pragma("page_size")?, pragma("page_count")?, pragma("freelist_count")?);
    // One row per page instead of dbstat's per-tree totals: a tree total is
    // computed in a single step that an interrupt cannot end.
    let mut statement = connection.prepare("SELECT name, pgsize, payload FROM dbstat('main')")?;
    let mut rows = statement.query([])?;
    let mut sizes = HashMap::<String, (u64, u64)>::new();
    while let Some(row) = rows.next()? {
        let size = sizes.entry(row.get(0)?).or_default();
        size.0 += nonnegative(row.get(1)?);
        size.1 += nonnegative(row.get(2)?);
    }
    let mut trees = sizes.into_iter().collect::<Vec<_>>();
    trees.sort_by(|(left, (left_bytes, _)), (right, (right_bytes, _))| {
        right_bytes.cmp(left_bytes).then_with(|| left.cmp(right))
    });
    let listed = trees
        .iter()
        .take(LISTED)
        .map(|(tree, (bytes, payload))| format!("{tree} {} (payload {})", mib(*bytes), mib(*payload)))
        .collect::<Vec<_>>()
        .join(", ");
    let rest: u64 = trees.iter().skip(LISTED).map(|(_, (bytes, _))| bytes).sum();
    Ok(format!(
        "storage breakdown {}: {} in {} b-trees, free {}; {listed}; other {}",
        name(path),
        mib(pages * page_size),
        trees.len(),
        mib(free * page_size),
        mib(rest),
    ))
}

fn open(path: &Path) -> rusqlite::Result<Connection> {
    let flags = super::snapshot::revision_reader_open_flags_for_target(cfg!(target_os = "android"));
    let connection = crate::sqlite_open::open_with_flags(path, flags)?;
    connection.execute_batch("PRAGMA query_only = ON;")?;
    Ok(connection)
}

fn nonnegative(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn name(path: &Path) -> String {
    path.file_name().map_or_else(String::new, |name| name.to_string_lossy().into_owned())
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_breakdown_lists_the_largest_trees_first_with_free_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("synthetic.sqlite");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE large(body BLOB); CREATE TABLE small(body BLOB); CREATE TABLE dropped(body BLOB);",
        ).unwrap();
        for (table, count) in [("large", 64), ("small", 4), ("dropped", 64)] {
            for _ in 0..count {
                writer.execute(&format!("INSERT INTO {table} VALUES(zeroblob(65536))"), []).unwrap();
            }
        }
        writer.execute_batch("DROP TABLE dropped;").unwrap();
        let line = breakdown(&open(&path).unwrap(), &path).unwrap();
        assert!(line.starts_with("storage breakdown synthetic.sqlite: "), "{line}");
        let (large, small) = (line.find(", large ").or_else(|| line.find("; large ")).unwrap(), line.find(", small ").unwrap());
        assert!(large < small, "{line}");
        let entry = &line[large + 2..];
        assert!(entry[..entry.find(')').unwrap() + 1].ends_with("(payload 4.0 MiB)"), "{line}");
        assert!(!line.contains("free 0.0 MiB"), "the dropped table's pages were not reported free: {line}");
        drop(writer);
    }
}
