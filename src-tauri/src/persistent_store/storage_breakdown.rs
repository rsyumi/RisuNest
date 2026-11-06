//! Records which tables and indexes fill the store files, once per process, so a
//! report of an unexpectedly large database can be answered from the log.

use rusqlite::Connection;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

static STARTED: AtomicBool = AtomicBool::new(false);
const LISTED: usize = 12;

/// Logs the breakdown of `files` on a thread of its own, unless this process
/// already started one. Reading every page of a large store takes a while, so
/// it never runs on the caller's thread or under the store lock.
pub(super) fn log_once(files: Vec<PathBuf>) {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("pds-storage-breakdown".to_owned())
        .spawn(move || {
            for path in files {
                match breakdown(&path) {
                    Ok(line) => crate::nlog!("info", "{line}"),
                    Err(error) => crate::nlog!("warn", "storage breakdown of {} failed: {error}", name(&path)),
                }
            }
        });
    if spawned.is_err() {
        STARTED.store(false, Ordering::Release);
    }
}

/// One line naming the file's size, its free pages and its largest b-trees.
fn breakdown(path: &Path) -> rusqlite::Result<String> {
    let connection = open(path)?;
    let pragma = |name: &str| -> rusqlite::Result<u64> {
        connection.query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0)).map(nonnegative)
    };
    let (page_size, pages, free) = (pragma("page_size")?, pragma("page_count")?, pragma("freelist_count")?);
    let mut statement = connection.prepare(
        "SELECT name, pgsize, payload FROM dbstat('main', 1) ORDER BY pgsize DESC, name",
    )?;
    let trees = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, nonnegative(row.get(1)?), nonnegative(row.get(2)?))))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let listed = trees
        .iter()
        .take(LISTED)
        .map(|(tree, bytes, payload)| format!("{tree} {} (payload {})", mib(*bytes), mib(*payload)))
        .collect::<Vec<_>>()
        .join(", ");
    let rest: u64 = trees.iter().skip(LISTED).map(|(_, bytes, _)| bytes).sum();
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
        let line = breakdown(&path).unwrap();
        assert!(line.starts_with("storage breakdown synthetic.sqlite: "), "{line}");
        let (large, small) = (line.find(", large ").or_else(|| line.find("; large ")).unwrap(), line.find(", small ").unwrap());
        assert!(large < small, "{line}");
        let entry = &line[large + 2..];
        assert!(entry[..entry.find(')').unwrap() + 1].ends_with("(payload 4.0 MiB)"), "{line}");
        assert!(!line.contains("free 0.0 MiB"), "the dropped table's pages were not reported free: {line}");
        drop(writer);
    }
}
