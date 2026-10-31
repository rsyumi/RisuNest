//! Shrinks a store file that consists mostly of free pages. SQLite reuses the
//! pages that deletions free but never returns them to the file system.

use super::{StoreError, StoreResult};
use rusqlite::{backup::{Backup, StepResult}, Connection};
use std::path::Path;

/// When a store file is compacted.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CompactionLimits {
    /// Free pages a file needs, in bytes. They must also be at least the used pages.
    pub(crate) min_free_bytes: u64,
    /// Used pages a file may have, in bytes. Compaction writes about three
    /// times this much before the store finishes opening.
    pub(crate) max_used_bytes: u64,
}

pub(crate) const STARTUP_COMPACTION: CompactionLimits = CompactionLimits {
    min_free_bytes: 1 << 30,
    max_used_bytes: 1 << 30,
};

/// What one compaction check found, in bytes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Compaction {
    pub(crate) used_bytes: u64,
    pub(crate) free_bytes: u64,
}

/// Copies the main database of `connection` into a scratch file without its free
/// pages and copies that back, so the file keeps only its used pages; content,
/// rowids and the header values stay the same. The caller makes sure no other
/// connection writes the file meanwhile, since the copy back replaces it.
/// Returns what it found when it compacted, and `None` when the file does not
/// meet `limits` or the volume lacks room for the copies.
pub(super) fn compact_if_sparse(
    connection: &mut Connection,
    repository_root: &Path,
    limits: CompactionLimits,
    available_space: impl FnOnce(&Path) -> std::io::Result<u64>,
) -> StoreResult<Option<Compaction>> {
    let pragma = |name: &str| -> StoreResult<u64> {
        let value: i64 = connection.query_row(&format!("PRAGMA {name}"), [], |row| row.get(0))?;
        u64::try_from(value).map_err(|_| StoreError::Store { message: format!("{name} is negative") })
    };
    let (page_size, page_count, free_count) = (pragma("page_size")?, pragma("page_count")?, pragma("freelist_count")?);
    let free_bytes = free_count.saturating_mul(page_size);
    let used_bytes = page_count.saturating_sub(free_count).saturating_mul(page_size);
    if free_bytes < limits.min_free_bytes || free_bytes < used_bytes || used_bytes > limits.max_used_bytes {
        return Ok(None);
    }
    let scratch = crate::external_storage::leftovers::managed_scratch(repository_root, "store-compaction-")
        .map_err(|error| StoreError::Store { message: error.to_string() })?;
    if available_space(scratch.path())? < used_bytes.saturating_mul(2) {
        return Ok(None);
    }
    let copy = scratch.path().join("compacted.sqlite");
    let name = crate::sqlite_open::sqlite_path(&copy).to_str()
        .ok_or_else(|| StoreError::Store { message: "compaction path is not valid UTF-8".to_owned() })?
        .to_owned();
    connection.execute("VACUUM INTO ?1", [name])?;
    {
        let source = crate::sqlite_open::open(&copy)?;
        // One step holds the write lock for the whole copy; a failure rolls it back.
        let backup = Backup::new(&source, connection)?;
        if backup.step(-1)? != StepResult::Done {
            return Err(StoreError::Store { message: "store compaction did not finish".to_owned() });
        }
    }
    connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    Ok(Some(Compaction { used_bytes, free_bytes }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIMITS: CompactionLimits = CompactionLimits { min_free_bytes: 1 << 20, max_used_bytes: 1 << 20 };

    fn database(root: &Path) -> Connection {
        let connection = crate::sqlite_open::open(root.join("store.sqlite")).unwrap();
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA user_version=7; PRAGMA application_id=1234;
            CREATE TABLE kept(value TEXT NOT NULL);
            CREATE TABLE bodies(hash TEXT PRIMARY KEY, body BLOB NOT NULL);
            CREATE INDEX body_sizes ON bodies(length(body));").unwrap();
        let tx = connection.unchecked_transaction().unwrap();
        for index in 0..400 {
            tx.execute("INSERT INTO bodies VALUES(?1,?2)", rusqlite::params![format!("synthetic-{index}"), vec![7u8; 8_192]]).unwrap();
        }
        for index in 0..50 {
            tx.execute("INSERT INTO kept VALUES(?1)", [format!("synthetic-kept-{index}")]).unwrap();
        }
        tx.execute("DELETE FROM kept WHERE rowid % 3 = 0", []).unwrap();
        tx.execute("DELETE FROM bodies WHERE hash<>'synthetic-7'", []).unwrap();
        tx.commit().unwrap();
        connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())).unwrap();
        connection
    }

    /// `kept` has no index, so it shows whether the copy renumbered rowids, as a
    /// plain `VACUUM` does for such a table.
    fn snapshot(connection: &Connection) -> (Vec<(i64, String)>, Vec<String>, i64, i64, String) {
        let rows = connection.prepare("SELECT rowid,value FROM kept ORDER BY rowid").unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
        let schema = connection.prepare("SELECT sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name").unwrap()
            .query_map([], |row| row.get(0)).unwrap().collect::<Result<_, _>>().unwrap();
        let read = |sql: &str| connection.query_row(sql, [], |row| row.get::<_, i64>(0)).unwrap();
        (rows, schema, read("PRAGMA user_version"), read("PRAGMA application_id"),
            connection.query_row("PRAGMA journal_mode", [], |row| row.get(0)).unwrap())
    }

    fn scratch_entries(root: &Path) -> usize {
        std::fs::read_dir(root.join("external-storage").join("scratch")).map_or(0, |entries| entries.count())
    }

    #[test]
    fn a_sparse_file_keeps_its_rows_rowids_and_header_and_drops_its_free_pages() {
        let root = tempfile::tempdir().unwrap();
        let mut connection = database(root.path());
        let before = snapshot(&connection);
        let size = || std::fs::metadata(root.path().join("store.sqlite")).unwrap().len();
        let grown = size();
        let compaction = compact_if_sparse(&mut connection, root.path(), LIMITS, |_| Ok(u64::MAX)).unwrap().unwrap();
        assert!(compaction.free_bytes >= LIMITS.min_free_bytes);
        assert_eq!(snapshot(&connection), before);
        assert_eq!(connection.query_row("PRAGMA freelist_count", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        assert!(size() <= compaction.used_bytes && size() < grown / 4, "{} of {grown}", size());
        assert_eq!(std::fs::metadata(root.path().join("store.sqlite-wal")).unwrap().len(), 0);
        connection.execute("INSERT INTO kept VALUES('synthetic-after')", []).unwrap();
        drop(connection);
        let reopened = crate::sqlite_open::open(root.path().join("store.sqlite")).unwrap();
        assert_eq!(reopened.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0)).unwrap(), "ok");
        assert_eq!(snapshot(&reopened).0.len(), before.0.len() + 1);
        assert_eq!(scratch_entries(root.path()), 0, "the scratch copy was left behind");
    }

    #[test]
    fn a_file_outside_the_limits_or_without_room_for_the_copies_is_left_as_it_is() {
        let root = tempfile::tempdir().unwrap();
        let mut connection = database(root.path());
        let pages = |connection: &Connection| connection.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0)).unwrap();
        let grown = pages(&connection);
        let refusals = [
            CompactionLimits { min_free_bytes: 1 << 30, ..LIMITS },
            CompactionLimits { max_used_bytes: 4_096, ..LIMITS },
        ];
        for limits in refusals {
            assert_eq!(compact_if_sparse(&mut connection, root.path(), limits, |_| Ok(u64::MAX)).unwrap(), None);
        }
        assert_eq!(compact_if_sparse(&mut connection, root.path(), LIMITS, |_| Ok(0)).unwrap(), None);
        assert_eq!(pages(&connection), grown);
        assert_eq!(scratch_entries(root.path()), 0);
    }

    #[test]
    fn a_failed_copy_leaves_the_file_unchanged_and_no_scratch_behind() {
        let root = tempfile::tempdir().unwrap();
        let mut connection = database(root.path());
        let before = snapshot(&connection);
        let pages = |connection: &Connection| connection.query_row("PRAGMA page_count", [], |row| row.get::<_, i64>(0)).unwrap();
        let grown = pages(&connection);
        let reader = crate::sqlite_open::open(root.path().join("store.sqlite")).unwrap();
        reader.execute_batch("BEGIN IMMEDIATE").unwrap();
        connection.busy_timeout(std::time::Duration::ZERO).unwrap();
        assert!(compact_if_sparse(&mut connection, root.path(), LIMITS, |_| Ok(u64::MAX)).is_err());
        reader.execute_batch("ROLLBACK").unwrap();
        assert_eq!(snapshot(&connection), before);
        assert_eq!(pages(&connection), grown);
        assert_eq!(scratch_entries(root.path()), 0);
    }
}
