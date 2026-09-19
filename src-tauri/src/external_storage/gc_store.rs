//! Reconstructible unreachable observations and cleanup statistics.
//! Neither lease ownership nor permission to delete survives in this database.
use super::contract::{ErrorKind, ProviderError, RemoteLocator, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::{collections::BTreeMap, path::Path};

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn storage(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}
fn count(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| corrupt())
}
fn amount(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| corrupt())
}
pub(crate) fn locator_key(locator: &RemoteLocator) -> Result<String> {
    serde_json::to_string(locator).map_err(|_| corrupt())
}

pub(crate) struct GcStore(Connection);

impl GcStore {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(storage)?;
        if crate::trust_boundary::is_link_like(&std::fs::symlink_metadata(root).map_err(storage)?) {
            return Err(corrupt());
        }
        let path = root.join("external-gc.sqlite");
        match std::fs::OpenOptions::new().create_new(true).write(true).open(&path) {
            Ok(file) => file.sync_all().map_err(storage)?,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                crate::trust_boundary::open_regular_source(&path).map_err(storage)?;
            }
            Err(error) => return Err(storage(error)),
        }
        let db = Connection::open(path).map_err(storage)?;
        db.busy_timeout(std::time::Duration::from_secs(5)).map_err(storage)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS observations(
               connection_id TEXT NOT NULL,
               target_id TEXT NOT NULL,
               identity TEXT NOT NULL,
               first_unreachable_ms INTEGER NOT NULL CHECK(first_unreachable_ms>=0),
               last_observed_ms INTEGER NOT NULL CHECK(last_observed_ms>=first_unreachable_ms),
               PRIMARY KEY(connection_id,target_id));
             CREATE TABLE IF NOT EXISTS cleanup_state(
               connection_id TEXT PRIMARY KEY,
               observation_valid INTEGER NOT NULL DEFAULT 0 CHECK(observation_valid IN (0,1)),
               last_run_ms INTEGER,
               stopped_reason TEXT,
               last_reachable_bytes INTEGER,
               last_removed_count INTEGER,
               last_removed_bytes INTEGER);
             CREATE TABLE IF NOT EXISTS inventory_page_observations(
               connection_id TEXT NOT NULL,
               target_id TEXT NOT NULL,
               identity TEXT NOT NULL,
               first_absent_ms INTEGER NOT NULL CHECK(first_absent_ms>=0),
               last_observed_ms INTEGER NOT NULL CHECK(last_observed_ms>=first_absent_ms),
               PRIMARY KEY(connection_id,target_id));",
        ).map_err(storage)?;
        for (table, expected) in [
            ("observations", &[
                "connection_id", "target_id", "identity", "first_unreachable_ms", "last_observed_ms",
            ][..]),
            ("cleanup_state", &[
                "connection_id", "observation_valid", "last_run_ms", "stopped_reason",
                "last_reachable_bytes", "last_removed_count", "last_removed_bytes",
            ][..]),
            ("inventory_page_observations", &[
                "connection_id", "target_id", "identity", "first_absent_ms", "last_observed_ms",
            ][..]),
        ] {
            let mut query = db.prepare(&format!("PRAGMA table_info({table})")).map_err(storage)?;
            let columns = query.query_map([], |row| row.get::<_, String>(1))
                .map_err(storage)?.collect::<std::result::Result<Vec<_>, _>>().map_err(storage)?;
            if columns != expected {
                return Err(corrupt());
            }
        }
        Ok(Self(db))
    }

    /// A crash or incomplete survey must break the previous unreachable interval.
    /// Only the execution that owns repository protection starts an observation.
    pub(crate) fn begin_observation(&self, connection_id: &str) -> Result<()> {
        let transaction = self.0.unchecked_transaction().map_err(storage)?;
        let valid = transaction.query_row(
            "SELECT observation_valid FROM cleanup_state WHERE connection_id=?1",
            [connection_id], |row| row.get::<_, bool>(0),
        ).optional().map_err(storage)?.unwrap_or(false);
        if !valid {
            transaction.execute("DELETE FROM observations WHERE connection_id=?1", [connection_id])
                .map_err(storage)?;
            transaction.execute(
                "DELETE FROM inventory_page_observations WHERE connection_id=?1",
                [connection_id],
            ).map_err(storage)?;
        }
        transaction.execute(
            "INSERT INTO cleanup_state(connection_id,observation_valid) VALUES(?1,0)
             ON CONFLICT(connection_id) DO UPDATE SET observation_valid=0",
            [connection_id],
        ).map_err(storage)?;
        transaction.commit().map_err(storage)
    }

    pub(crate) fn finish_observation(&self, connection_id: &str) -> Result<()> {
        if self.0.execute(
            "UPDATE cleanup_state SET observation_valid=1
             WHERE connection_id=?1 AND observation_valid=0",
            [connection_id],
        ).map_err(storage)? != 1 {
            return Err(corrupt());
        }
        Ok(())
    }

    pub(crate) fn invalidate_observations(&self, connection_id: &str) -> Result<()> {
        let transaction = self.0.unchecked_transaction().map_err(storage)?;
        transaction.execute("DELETE FROM observations WHERE connection_id=?1", [connection_id])
            .map_err(storage)?;
        transaction.execute(
            "DELETE FROM inventory_page_observations WHERE connection_id=?1",
            [connection_id],
        ).map_err(storage)?;
        transaction.execute(
            "INSERT INTO cleanup_state(connection_id,observation_valid) VALUES(?1,0)
             ON CONFLICT(connection_id) DO UPDATE SET observation_valid=0",
            [connection_id],
        ).map_err(storage)?;
        transaction.commit().map_err(storage)
    }

    /// The input is the complete unreachable set, keyed by locator and immutable
    /// identity. Reachable, absent, replaced and backward-clock entries lose age.
    pub(crate) fn record_observations(
        &self,
        connection_id: &str,
        unreachable: &BTreeMap<String, String>,
        now_ms: u64,
    ) -> Result<BTreeMap<String, u64>> {
        let now = count(now_ms)?;
        let transaction = self.0.unchecked_transaction().map_err(storage)?;
        let previous = {
            let mut query = transaction.prepare(
                "SELECT target_id,identity,first_unreachable_ms,last_observed_ms
                 FROM observations WHERE connection_id=?1",
            ).map_err(storage)?;
            let rows = query.query_map([connection_id], |row| Ok((
                row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?, row.get::<_, i64>(3)?,
            ))).map_err(storage)?.collect::<std::result::Result<Vec<_>, _>>().map_err(storage)?;
            rows
        };
        let mut ages = BTreeMap::new();
        for (target, identity, first, last) in previous {
            if first < 0 || last < first {
                return Err(corrupt());
            }
            if unreachable.get(&target) == Some(&identity) && now >= last {
                ages.insert(target, amount(first)?);
            }
        }
        transaction.execute("DELETE FROM observations WHERE connection_id=?1", [connection_id])
            .map_err(storage)?;
        for (target, identity) in unreachable {
            if target.is_empty() || !crate::trust_boundary::is_lower_hex_256(identity) {
                return Err(corrupt());
            }
            let first = *ages.entry(target.clone()).or_insert(now_ms);
            transaction.execute(
                "INSERT INTO observations VALUES(?1,?2,?3,?4,?5)",
                params![connection_id, target, identity, count(first)?, now],
            ).map_err(storage)?;
        }
        transaction.commit().map_err(storage)?;
        Ok(ages)
    }

    pub(crate) fn forget_observation(&self, connection_id: &str, target: &str) -> Result<()> {
        self.0.execute(
            "DELETE FROM observations WHERE connection_id=?1 AND target_id=?2",
            params![connection_id, target],
        ).map_err(storage)?;
        Ok(())
    }

    pub(crate) fn record_inventory_page_observations(
        &self,
        connection_id: &str,
        absent: &BTreeMap<String, String>,
        now_ms: u64,
    ) -> Result<BTreeMap<String, u64>> {
        let now = count(now_ms)?;
        let transaction = self.0.unchecked_transaction().map_err(storage)?;
        let previous = {
            let mut query = transaction.prepare(
                "SELECT target_id,identity,first_absent_ms,last_observed_ms
                 FROM inventory_page_observations WHERE connection_id=?1",
            ).map_err(storage)?;
            let rows = query.query_map([connection_id], |row| Ok((
                row.get::<_, String>(0)?, row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?, row.get::<_, i64>(3)?,
            )))
            .map_err(storage)?
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(storage)?;
            rows
        };
        let mut ages = BTreeMap::new();
        for (target, identity, first, last) in previous {
            if first < 0 || last < first {
                return Err(corrupt());
            }
            if absent.get(&target) == Some(&identity) && now >= last {
                ages.insert(target, amount(first)?);
            }
        }
        transaction.execute(
            "DELETE FROM inventory_page_observations WHERE connection_id=?1",
            [connection_id],
        ).map_err(storage)?;
        for (target, identity) in absent {
            if target.is_empty() || !crate::trust_boundary::is_lower_hex_256(identity) {
                return Err(corrupt());
            }
            let first = *ages.entry(target.clone()).or_insert(now_ms);
            transaction.execute(
                "INSERT INTO inventory_page_observations VALUES(?1,?2,?3,?4,?5)",
                params![connection_id, target, identity, count(first)?, now],
            ).map_err(storage)?;
        }
        transaction.commit().map_err(storage)?;
        Ok(ages)
    }

    pub(crate) fn forget_inventory_page_observation(
        &self,
        connection_id: &str,
        target: &str,
    ) -> Result<()> {
        self.0.execute(
            "DELETE FROM inventory_page_observations WHERE connection_id=?1 AND target_id=?2",
            params![connection_id, target],
        ).map_err(storage)?;
        Ok(())
    }

    pub(crate) fn last_reachable_bytes(&self, connection_id: &str) -> Result<Option<u64>> {
        let value = self.0.query_row(
            "SELECT last_reachable_bytes FROM cleanup_state WHERE connection_id=?1",
            [connection_id], |row| row.get::<_, Option<i64>>(0),
        ).optional().map_err(storage)?.flatten();
        value.map(amount).transpose()
    }

    pub(crate) fn set_last_reachable_bytes(&self, connection_id: &str, bytes: u64) -> Result<()> {
        self.0.execute(
            "INSERT INTO cleanup_state(connection_id,last_reachable_bytes) VALUES(?1,?2)
             ON CONFLICT(connection_id) DO UPDATE SET last_reachable_bytes=excluded.last_reachable_bytes",
            params![connection_id, count(bytes)?],
        ).map_err(storage)?;
        Ok(())
    }

    pub(crate) fn record_cleanup_run(
        &self,
        connection_id: &str,
        now_ms: u64,
        reason: &str,
        removed_count: u64,
        removed_bytes: u64,
    ) -> Result<()> {
        self.0.execute(
            "INSERT INTO cleanup_state(connection_id,last_run_ms,stopped_reason,last_removed_count,last_removed_bytes)
             VALUES(?1,?2,?3,?4,?5)
             ON CONFLICT(connection_id) DO UPDATE SET last_run_ms=excluded.last_run_ms,
               stopped_reason=excluded.stopped_reason,last_removed_count=excluded.last_removed_count,
               last_removed_bytes=excluded.last_removed_bytes",
            params![connection_id, count(now_ms)?, reason, count(removed_count)?, count(removed_bytes)?],
        ).map_err(storage)?;
        Ok(())
    }

    pub(crate) fn forget_connection(&self, connection_id: &str) -> Result<()> {
        let transaction = self.0.unchecked_transaction().map_err(storage)?;
        transaction.execute("DELETE FROM observations WHERE connection_id=?1", [connection_id])
            .map_err(storage)?;
        transaction.execute(
            "DELETE FROM inventory_page_observations WHERE connection_id=?1",
            [connection_id],
        ).map_err(storage)?;
        transaction.execute("DELETE FROM cleanup_state WHERE connection_id=?1", [connection_id])
            .map_err(storage)?;
        transaction.commit().map_err(storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn targets(values: &[(&str, &str)]) -> BTreeMap<String, String> {
        values.iter().map(|(key, identity)| (
            key.to_string(), risunest_sync_wire::hash(identity.as_bytes()),
        )).collect()
    }

    #[test]
    fn c_only_complete_observations_preserve_age_across_restart() {
        let root = tempfile::tempdir().unwrap();
        let objects = targets(&[("a", "one"), ("b", "two")]);
        let store = GcStore::open(root.path()).unwrap();
        store.begin_observation("connection").unwrap();
        assert_eq!(store.record_observations("connection", &objects, 1000).unwrap()["a"], 1000);
        store.finish_observation("connection").unwrap();
        drop(store);
        let store = GcStore::open(root.path()).unwrap();
        store.begin_observation("connection").unwrap();
        assert_eq!(store.record_observations("connection", &objects, 2000).unwrap()["a"], 1000);
        drop(store);
        let store = GcStore::open(root.path()).unwrap();
        store.begin_observation("connection").unwrap();
        assert_eq!(store.record_observations("connection", &objects, 3000).unwrap()["a"], 3000);
    }

    #[test]
    fn c_reachable_absent_changed_and_backward_clock_entries_reset_age() {
        let root = tempfile::tempdir().unwrap();
        let store = GcStore::open(root.path()).unwrap();
        store.begin_observation("connection").unwrap();
        store.record_observations("connection", &targets(&[("a", "one"), ("b", "two")]), 1000).unwrap();
        let later = store.record_observations("connection", &targets(&[("a", "changed")]), 2000).unwrap();
        assert_eq!(later["a"], 2000);
        assert!(!later.contains_key("b"));
        let again = store.record_observations("connection", &targets(&[("a", "changed"), ("b", "two")]), 3000).unwrap();
        assert_eq!(again["a"], 2000);
        assert_eq!(again["b"], 3000);
        assert_eq!(store.record_observations("connection", &targets(&[("a", "changed")]), 1500).unwrap()["a"], 1500);
    }

    #[test]
    fn c_invalid_observation_is_atomic_and_connections_are_independent() {
        let root = tempfile::tempdir().unwrap();
        let store = GcStore::open(root.path()).unwrap();
        let objects = targets(&[("a", "one")]);
        store.begin_observation("a").unwrap();
        store.begin_observation("b").unwrap();
        store.record_observations("a", &objects, 1000).unwrap();
        store.record_observations("b", &objects, 2000).unwrap();
        assert!(store.record_observations("a", &[("a".into(), "bad".into())].into(), 3000).is_err());
        assert_eq!(store.record_observations("a", &objects, 4000).unwrap()["a"], 1000);
        store.invalidate_observations("a").unwrap();
        assert_eq!(store.record_observations("a", &objects, 5000).unwrap()["a"], 5000);
        assert_eq!(store.record_observations("b", &objects, 5000).unwrap()["a"], 2000);
        assert!(store.record_observations("b", &objects, u64::MAX).is_err());
    }

    #[test]
    fn c_only_observations_and_statistics_survive_completion() {
        let root = tempfile::tempdir().unwrap();
        let store = GcStore::open(root.path()).unwrap();
        let mut query = store.0.prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name").unwrap();
        let tables = query.query_map([], |row| row.get::<_, String>(0)).unwrap()
            .collect::<std::result::Result<Vec<_>, _>>().unwrap();
        assert_eq!(tables, ["cleanup_state", "inventory_page_observations", "observations"]);
        assert_eq!(store.last_reachable_bytes("connection").unwrap(), None);
        store.set_last_reachable_bytes("connection", 512).unwrap();
        store.record_cleanup_run("connection", 1000, "complete", 1, 256).unwrap();
        assert_eq!(store.last_reachable_bytes("connection").unwrap(), Some(512));
        store.forget_connection("connection").unwrap();
        assert_eq!(store.last_reachable_bytes("connection").unwrap(), None);
    }

    #[test]
    fn c_incompatible_observation_format_is_rejected_without_migration() {
        let root = tempfile::tempdir().unwrap();
        let db = Connection::open(root.path().join("external-gc.sqlite")).unwrap();
        db.execute_batch("CREATE TABLE observations(connection_id TEXT,target_id TEXT,first_seen_ms INTEGER);").unwrap();
        drop(db);
        assert!(matches!(GcStore::open(root.path()), Err(error) if error.kind == ErrorKind::Corrupt));
    }

    #[test]
    fn inventory_page_age_survives_only_complete_unchanged_observations() {
        let root = tempfile::tempdir().unwrap();
        let page = targets(&[("page", "identity")]);
        let changed = targets(&[("page", "changed")]);
        let store = GcStore::open(root.path()).unwrap();
        store.begin_observation("connection").unwrap();
        assert_eq!(store.record_inventory_page_observations("connection", &page, 1000).unwrap()["page"], 1000);
        store.finish_observation("connection").unwrap();
        drop(store);

        let store = GcStore::open(root.path()).unwrap();
        store.begin_observation("connection").unwrap();
        assert_eq!(store.record_inventory_page_observations("connection", &page, 2000).unwrap()["page"], 1000);
        assert_eq!(store.record_inventory_page_observations("connection", &changed, 3000).unwrap()["page"], 3000);
        store.invalidate_observations("connection").unwrap();
        assert_eq!(store.record_inventory_page_observations("connection", &page, 4000).unwrap()["page"], 4000);
    }
}
