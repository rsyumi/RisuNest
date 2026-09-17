//! Cleanup bookkeeping that has to outlive a single run: the local time this
//! device first saw a remote target, what a run committed to remove before it
//! sent its first request, and the per-connection record the usage screen
//! reads instead of walking the repository again.
// The run that writes these tables is wired with the removal path; the usage
// summary is the only reader until then.
use super::contract::{ErrorKind, LeaseKind, ObjectRole, ProviderError, RemoteLocator, Result};
use rusqlite::{Connection, OptionalExtension};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn storage(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

/// The key every cleanup table uses for a remote object. The struct fixes the
/// field order and nothing is skipped, so two equal locators always encode to
/// the same bytes whether they came from an enumeration or from a catalog.
pub(crate) fn locator_key(locator: &RemoteLocator) -> Result<String> {
    serde_json::to_string(locator).map_err(|_| corrupt())
}
fn decode_locator(value: &str) -> Result<RemoteLocator> {
    serde_json::from_str(value).map_err(|_| corrupt())
}
fn role_name(role: ObjectRole) -> Result<String> {
    match serde_json::to_value(role).map_err(|_| corrupt())? {
        serde_json::Value::String(name) => Ok(name),
        _ => Err(corrupt()),
    }
}
fn decode_role(value: &str) -> Result<ObjectRole> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|_| corrupt())
}
fn decode_lease_kind(value: &str) -> Result<LeaseKind> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(|_| corrupt())
}
fn count(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| corrupt())
}
fn amount(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| corrupt())
}

/// One target a run decided to remove, written before the first request so an
/// interrupted run can still name the fragments its parent document hid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommittedDeletion {
    pub locator: RemoteLocator,
    pub role: ObjectRole,
    pub byte_length: u64,
    pub decided_at_ms: u64,
    pub done: bool,
}

/// How far one lease got. A `Pending` row names bytes that may or may not have
/// reached the repository; a `Releasing` row names an object this device has
/// decided to remove but has not seen removed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LeaseState {
    Pending,
    Confirmed,
    Releasing,
}
impl LeaseState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Confirmed => "confirmed",
            Self::Releasing => "releasing",
        }
    }
    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "pending" => Self::Pending,
            "confirmed" => Self::Confirmed,
            "releasing" => Self::Releasing,
            _ => return Err(corrupt()),
        })
    }
}

/// One lease this device decided to place, written before the request that
/// places it. `bytes` is the sealed object exactly as it was first sent, so a
/// retry after an unclear answer writes the same name with the same content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeaseIntent {
    pub locator: RemoteLocator,
    pub kind: LeaseKind,
    pub job_id: String,
    pub seq: u64,
    pub bytes: Vec<u8>,
    pub state: LeaseState,
    pub created_at_ms: u64,
}

/// One removal request this device sent. The row outlives the request: it is
/// only marked finished once the remote end is known, never after a local
/// timeout, a cancellation or a restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DeleteRequest {
    pub locator: RemoteLocator,
    pub attempt_id: String,
    pub sent_at_ms: u64,
    pub finished: bool,
}

pub(crate) struct GcStore(Connection);

impl GcStore {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(storage)?;
        if crate::trust_boundary::is_link_like(&std::fs::symlink_metadata(root).map_err(storage)?) {
            return Err(corrupt());
        }
        let path = root.join("external-gc.sqlite");
        if path.exists() {
            crate::trust_boundary::open_regular_source(&path).map_err(storage)?;
        }
        let db = Connection::open(path).map_err(storage)?;
        db.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(storage)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS observations(
               connection_id TEXT NOT NULL,
               target_id TEXT NOT NULL,
               first_seen_ms INTEGER NOT NULL,
               PRIMARY KEY(connection_id, target_id)
             );
             CREATE TABLE IF NOT EXISTS deletions(
               connection_id TEXT NOT NULL,
               locator TEXT NOT NULL,
               role TEXT NOT NULL,
               byte_length INTEGER NOT NULL,
               decided_at_ms INTEGER NOT NULL,
               done INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY(connection_id, locator)
             );
             CREATE TABLE IF NOT EXISTS lease_intents(
               connection_id TEXT NOT NULL,
               locator TEXT NOT NULL,
               kind TEXT NOT NULL,
               job_id TEXT NOT NULL,
               seq INTEGER NOT NULL,
               bytes BLOB NOT NULL,
               state TEXT NOT NULL,
               created_at_ms INTEGER NOT NULL,
               PRIMARY KEY(connection_id, locator)
             );
             CREATE TABLE IF NOT EXISTS delete_requests(
               connection_id TEXT NOT NULL,
               locator TEXT NOT NULL,
               attempt_id TEXT NOT NULL,
               sent_at_ms INTEGER NOT NULL,
               finished INTEGER NOT NULL DEFAULT 0,
               PRIMARY KEY(connection_id, locator, attempt_id)
             );
             CREATE TABLE IF NOT EXISTS cleanup_state(
               connection_id TEXT PRIMARY KEY,
               last_run_ms INTEGER,
               publishes_since INTEGER NOT NULL DEFAULT 0,
               policy_changed INTEGER NOT NULL DEFAULT 0,
               stopped_reason TEXT,
               last_reachable_bytes INTEGER,
               last_removed_count INTEGER,
               last_removed_bytes INTEGER
             );",
        )
        .map_err(storage)?;
        Ok(Self(db))
    }

    /// Stamps the given targets and answers with every target this connection
    /// holds a row for, each with the local time it was first seen. A target
    /// with no row yet is recorded now, so something seen for the first time is
    /// always inside the grace window.
    pub(crate) fn record_observations(
        &self,
        connection_id: &str,
        targets: &BTreeSet<String>,
        now_ms: u64,
    ) -> Result<BTreeMap<String, u64>> {
        let stamp = count(now_ms)?;
        let transaction = self.0.unchecked_transaction().map_err(storage)?;
        {
            let mut insert = transaction
                .prepare(
                    "INSERT OR IGNORE INTO observations(connection_id,target_id,first_seen_ms)
                     VALUES(?1,?2,?3)",
                )
                .map_err(storage)?;
            for target in targets {
                insert
                    .execute(rusqlite::params![connection_id, target, stamp])
                    .map_err(storage)?;
            }
        }
        transaction.commit().map_err(storage)?;
        let mut query = self
            .0
            .prepare("SELECT target_id,first_seen_ms FROM observations WHERE connection_id=?1")
            .map_err(storage)?;
        let rows = query
            .query_map([connection_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })
            .map_err(storage)?;
        let mut seen = BTreeMap::new();
        for row in rows {
            let (target, first_seen) = row.map_err(storage)?;
            seen.insert(target, amount(first_seen)?);
        }
        Ok(seen)
    }

    /// Drops the rows of targets that left the enumeration. Only a run that
    /// read every root calls this; a partial view must not forget a target and
    /// restart its grace window from a later observation.
    pub(crate) fn prune_observations(
        &self,
        connection_id: &str,
        keep: &BTreeSet<String>,
    ) -> Result<()> {
        let transaction = self.0.unchecked_transaction().map_err(storage)?;
        {
            let mut query = transaction
                .prepare("SELECT target_id FROM observations WHERE connection_id=?1")
                .map_err(storage)?;
            let rows = query
                .query_map([connection_id], |row| row.get::<_, String>(0))
                .map_err(storage)?;
            let mut stale = Vec::new();
            for row in rows {
                let target = row.map_err(storage)?;
                if !keep.contains(&target) {
                    stale.push(target);
                }
            }
            let mut delete = transaction
                .prepare("DELETE FROM observations WHERE connection_id=?1 AND target_id=?2")
                .map_err(storage)?;
            for target in stale {
                delete
                    .execute(rusqlite::params![connection_id, target])
                    .map_err(storage)?;
            }
        }
        transaction.commit().map_err(storage)
    }

    /// Everything an earlier run committed to remove. A row this device can no
    /// longer decode makes the whole list unusable, which the caller answers by
    /// computing a fresh one.
    pub(crate) fn committed_deletions(&self, connection_id: &str) -> Result<Vec<CommittedDeletion>> {
        let mut query = self
            .0
            .prepare(
                "SELECT locator,role,byte_length,decided_at_ms,done FROM deletions
                 WHERE connection_id=?1 ORDER BY locator",
            )
            .map_err(storage)?;
        let rows = query
            .query_map([connection_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            })
            .map_err(storage)?;
        let mut result = Vec::new();
        for row in rows {
            let (locator, role, byte_length, decided_at_ms, done) = row.map_err(storage)?;
            result.push(CommittedDeletion {
                locator: decode_locator(&locator)?,
                role: decode_role(&role)?,
                byte_length: amount(byte_length)?,
                decided_at_ms: amount(decided_at_ms)?,
                done: done != 0,
            });
        }
        Ok(result)
    }

    /// Replaces the list with what this run committed to. One transaction, so a
    /// reader never sees a list that names neither the old nor the new targets.
    pub(crate) fn replace_deletions(
        &self,
        connection_id: &str,
        entries: &[CommittedDeletion],
    ) -> Result<()> {
        let transaction = self.0.unchecked_transaction().map_err(storage)?;
        transaction
            .execute("DELETE FROM deletions WHERE connection_id=?1", [connection_id])
            .map_err(storage)?;
        {
            let mut insert = transaction
                .prepare(
                    "INSERT INTO deletions(connection_id,locator,role,byte_length,decided_at_ms,done)
                     VALUES(?1,?2,?3,?4,?5,?6)",
                )
                .map_err(storage)?;
            for entry in entries {
                insert
                    .execute(rusqlite::params![
                        connection_id,
                        locator_key(&entry.locator)?,
                        role_name(entry.role)?,
                        count(entry.byte_length)?,
                        count(entry.decided_at_ms)?,
                        i64::from(entry.done),
                    ])
                    .map_err(storage)?;
            }
        }
        transaction.commit().map_err(storage)
    }

    /// Records that the remote end confirmed one target is gone. The row stays
    /// until the next run's mark, which is what lets a resumed run tell a
    /// finished target from one it never reached.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn mark_deletion_done(
        &self,
        connection_id: &str,
        locator: &RemoteLocator,
    ) -> Result<()> {
        self.0
            .execute(
                "UPDATE deletions SET done=1 WHERE connection_id=?1 AND locator=?2",
                rusqlite::params![connection_id, locator_key(locator)?],
            )
            .map_err(storage)?;
        Ok(())
    }

    pub(crate) fn last_reachable_bytes(&self, connection_id: &str) -> Result<Option<u64>> {
        let value: Option<Option<i64>> = self
            .0
            .query_row(
                "SELECT last_reachable_bytes FROM cleanup_state WHERE connection_id=?1",
                [connection_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        value.flatten().map(amount).transpose()
    }

    pub(crate) fn set_last_reachable_bytes(&self, connection_id: &str, bytes: u64) -> Result<()> {
        self.0
            .execute(
                "INSERT INTO cleanup_state(connection_id,last_reachable_bytes) VALUES(?1,?2)
                 ON CONFLICT(connection_id) DO UPDATE SET last_reachable_bytes=excluded.last_reachable_bytes",
                rusqlite::params![connection_id, count(bytes)?],
            )
            .map_err(storage)?;
        Ok(())
    }

    /// What the last run did and why it stopped. The automatic schedule reads
    /// this to tell a run that yielded from one that stopped at a bound, and
    /// `publishes_since` starts again from zero whenever a run ends.
    pub(crate) fn record_cleanup_run(
        &self,
        connection_id: &str,
        now_ms: u64,
        stopped_reason: &str,
        removed_count: u64,
        removed_bytes: u64,
    ) -> Result<()> {
        self.0
            .execute(
                "INSERT INTO cleanup_state(connection_id,last_run_ms,publishes_since,policy_changed,
                     stopped_reason,last_removed_count,last_removed_bytes)
                 VALUES(?1,?2,0,0,?3,?4,?5)
                 ON CONFLICT(connection_id) DO UPDATE SET
                     last_run_ms=excluded.last_run_ms,
                     publishes_since=0,
                     policy_changed=0,
                     stopped_reason=excluded.stopped_reason,
                     last_removed_count=excluded.last_removed_count,
                     last_removed_bytes=excluded.last_removed_bytes",
                rusqlite::params![
                    connection_id,
                    count(now_ms)?,
                    stopped_reason,
                    count(removed_count)?,
                    count(removed_bytes)?
                ],
            )
            .map_err(storage)?;
        Ok(())
    }

    /// Writes the intent of a lease this device is about to place. The row has
    /// to exist before the request leaves, or a lost answer would leave an
    /// object this device can neither name nor remove.
    pub(crate) fn put_lease_intent(&self, connection_id: &str, intent: &LeaseIntent) -> Result<()> {
        self.0
            .execute(
                "INSERT INTO lease_intents(connection_id,locator,kind,job_id,seq,bytes,state,created_at_ms)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                rusqlite::params![
                    connection_id,
                    locator_key(&intent.locator)?,
                    intent.kind.as_str(),
                    intent.job_id,
                    count(intent.seq)?,
                    intent.bytes,
                    intent.state.as_str(),
                    count(intent.created_at_ms)?,
                ],
            )
            .map_err(storage)?;
        Ok(())
    }

    /// Moves a placed lease to the locator the repository answered with and
    /// records that it exists. The two happen together: a confirmed row is the
    /// one an enumeration can be matched against.
    pub(crate) fn confirm_lease_intent(
        &self,
        connection_id: &str,
        placed: &RemoteLocator,
        confirmed: &RemoteLocator,
    ) -> Result<()> {
        let transaction = self.0.unchecked_transaction().map_err(storage)?;
        let moved = transaction
            .execute(
                "UPDATE lease_intents SET locator=?3, state='confirmed'
                 WHERE connection_id=?1 AND locator=?2",
                rusqlite::params![
                    connection_id,
                    locator_key(placed)?,
                    locator_key(confirmed)?
                ],
            )
            .map_err(storage)?;
        if moved != 1 {
            return Err(corrupt());
        }
        transaction.commit().map_err(storage)
    }

    pub(crate) fn set_lease_state(
        &self,
        connection_id: &str,
        locator: &RemoteLocator,
        state: LeaseState,
    ) -> Result<()> {
        self.0
            .execute(
                "UPDATE lease_intents SET state=?3 WHERE connection_id=?1 AND locator=?2",
                rusqlite::params![connection_id, locator_key(locator)?, state.as_str()],
            )
            .map_err(storage)?;
        Ok(())
    }

    /// Forgets a lease this device has seen removed from the repository.
    pub(crate) fn remove_lease_intent(
        &self,
        connection_id: &str,
        locator: &RemoteLocator,
    ) -> Result<()> {
        self.0
            .execute(
                "DELETE FROM lease_intents WHERE connection_id=?1 AND locator=?2",
                rusqlite::params![connection_id, locator_key(locator)?],
            )
            .map_err(storage)?;
        Ok(())
    }

    /// Every lease this device placed on this connection. What is not in here
    /// belongs to another device, whatever its name says.
    pub(crate) fn lease_intents(&self, connection_id: &str) -> Result<Vec<LeaseIntent>> {
        let mut query = self
            .0
            .prepare(
                "SELECT locator,kind,job_id,seq,bytes,state,created_at_ms FROM lease_intents
                 WHERE connection_id=?1 ORDER BY job_id,seq,locator",
            )
            .map_err(storage)?;
        let rows = query
            .query_map([connection_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            })
            .map_err(storage)?;
        let mut result = Vec::new();
        for row in rows {
            let (locator, kind, job_id, seq, bytes, state, created_at_ms) = row.map_err(storage)?;
            result.push(LeaseIntent {
                locator: decode_locator(&locator)?,
                kind: decode_lease_kind(&kind)?,
                job_id,
                seq: amount(seq)?,
                bytes,
                state: LeaseState::parse(&state)?,
                created_at_ms: amount(created_at_ms)?,
            });
        }
        Ok(result)
    }

    /// Records a removal request before it is sent. The row is what tells a
    /// later run that a request may still be running somewhere.
    pub(crate) fn record_delete_request(
        &self,
        connection_id: &str,
        locator: &RemoteLocator,
        attempt_id: &str,
        sent_at_ms: u64,
    ) -> Result<()> {
        self.0
            .execute(
                "INSERT OR IGNORE INTO delete_requests(connection_id,locator,attempt_id,sent_at_ms,finished)
                 VALUES(?1,?2,?3,?4,0)",
                rusqlite::params![
                    connection_id,
                    locator_key(locator)?,
                    attempt_id,
                    count(sent_at_ms)?
                ],
            )
            .map_err(storage)?;
        Ok(())
    }

    /// Records that the repository answered for one request. Only an answer
    /// does this; a local timeout, a cancellation or a restart does not.
    pub(crate) fn finish_delete_request(
        &self,
        connection_id: &str,
        locator: &RemoteLocator,
        attempt_id: &str,
    ) -> Result<()> {
        self.0
            .execute(
                "UPDATE delete_requests SET finished=1
                 WHERE connection_id=?1 AND locator=?2 AND attempt_id=?3",
                rusqlite::params![connection_id, locator_key(locator)?, attempt_id],
            )
            .map_err(storage)?;
        Ok(())
    }

    /// The requests whose remote end is still unknown. `attempt` narrows the
    /// answer to one removal attempt, which is what a marker is held for.
    pub(crate) fn unfinished_delete_requests(
        &self,
        connection_id: &str,
        attempt: Option<&str>,
    ) -> Result<Vec<DeleteRequest>> {
        let mut query = self
            .0
            .prepare(
                "SELECT locator,attempt_id,sent_at_ms FROM delete_requests
                 WHERE connection_id=?1 AND finished=0 AND (?2 IS NULL OR attempt_id=?2)
                 ORDER BY attempt_id,locator",
            )
            .map_err(storage)?;
        let rows = query
            .query_map(rusqlite::params![connection_id, attempt], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
            .map_err(storage)?;
        let mut result = Vec::new();
        for row in rows {
            let (locator, attempt_id, sent_at_ms) = row.map_err(storage)?;
            result.push(DeleteRequest {
                locator: decode_locator(&locator)?,
                attempt_id,
                sent_at_ms: amount(sent_at_ms)?,
                finished: false,
            });
        }
        Ok(result)
    }

    /// Drops the answered requests of one finished attempt. A request whose
    /// remote end is still unknown stays, so the marker above it stays too.
    pub(crate) fn forget_finished_delete_requests(
        &self,
        connection_id: &str,
        attempt_id: &str,
    ) -> Result<()> {
        self.0
            .execute(
                "DELETE FROM delete_requests
                 WHERE connection_id=?1 AND attempt_id=?2 AND finished=1",
                rusqlite::params![connection_id, attempt_id],
            )
            .map_err(storage)?;
        Ok(())
    }

    /// Removes everything this connection owns. A removed connection keeps no
    /// observation, no committed target and no outstanding request.
    pub(crate) fn forget_connection(&self, connection_id: &str) -> Result<()> {
        let transaction = self.0.unchecked_transaction().map_err(storage)?;
        for table in [
            "observations",
            "deletions",
            "lease_intents",
            "delete_requests",
            "cleanup_state",
        ] {
            transaction
                .execute(
                    &format!("DELETE FROM {table} WHERE connection_id=?1"),
                    [connection_id],
                )
                .map_err(storage)?;
        }
        transaction.commit().map_err(storage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, GcStore) {
        let root = tempfile::tempdir().unwrap();
        let store = GcStore::open(root.path()).unwrap();
        (root, store)
    }
    fn locator(collection: Option<&str>, object: &str) -> RemoteLocator {
        RemoteLocator {
            connection_identity: "synthetic-account/root".into(),
            collection: collection.map(str::to_owned),
            object: object.into(),
        }
    }
    fn targets(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    /// The key is a value encoding, so an enumerated receipt and a locator
    /// revived from a catalog reference address the same row.
    #[test]
    fn equal_locators_encode_to_the_same_key() {
        let listed = locator(Some("snapshots"), "snapshots/snapshot-a");
        let revived = RemoteLocator {
            connection_identity: "synthetic-account/root".into(),
            collection: Some("snapshots".into()),
            object: "snapshots/snapshot-a".into(),
        };
        assert_eq!(locator_key(&listed).unwrap(), locator_key(&revived).unwrap());
        assert_eq!(
            locator_key(&locator(None, "pack-a")).unwrap(),
            locator_key(&locator(None, "pack-a")).unwrap()
        );
        assert_ne!(
            locator_key(&locator(None, "pack-a")).unwrap(),
            locator_key(&locator(Some("packs"), "pack-a")).unwrap()
        );
    }

    #[test]
    fn a_target_seen_for_the_first_time_is_recorded_now_and_keeps_that_time() {
        let (_root, store) = store();
        let first = store
            .record_observations("connection", &targets(&["a", "b"]), 1_000)
            .unwrap();
        assert_eq!(first.get("a"), Some(&1_000));
        let later = store
            .record_observations("connection", &targets(&["a", "c"]), 9_000)
            .unwrap();
        assert_eq!(later.get("a"), Some(&1_000));
        assert_eq!(later.get("c"), Some(&9_000));
    }

    #[test]
    fn a_target_that_left_the_enumeration_loses_its_row() {
        let (_root, store) = store();
        store
            .record_observations("connection", &targets(&["a", "b"]), 1_000)
            .unwrap();
        store
            .prune_observations("connection", &targets(&["a"]))
            .unwrap();
        let again = store
            .record_observations("connection", &targets(&["a", "b"]), 5_000)
            .unwrap();
        assert_eq!(again.get("a"), Some(&1_000));
        assert_eq!(again.get("b"), Some(&5_000));
    }

    #[test]
    fn a_committed_list_survives_and_records_what_finished() {
        let (_root, store) = store();
        let entry = CommittedDeletion {
            locator: locator(Some("snapshots"), "snapshots/snapshot-a"),
            role: ObjectRole::SyncState,
            byte_length: 42,
            decided_at_ms: 7,
            done: false,
        };
        store.replace_deletions("connection", &[entry.clone()]).unwrap();
        assert_eq!(store.committed_deletions("connection").unwrap(), vec![entry.clone()]);
        store
            .mark_deletion_done("connection", &entry.locator)
            .unwrap();
        assert!(store.committed_deletions("connection").unwrap()[0].done);
        store.replace_deletions("connection", &[]).unwrap();
        assert!(store.committed_deletions("connection").unwrap().is_empty());
    }

    #[test]
    fn an_undecodable_row_makes_the_whole_list_unusable() {
        let (_root, store) = store();
        store
            .0
            .execute(
                "INSERT INTO deletions(connection_id,locator,role,byte_length,decided_at_ms,done)
                 VALUES('connection','not-a-locator','pack',1,1,0)",
                [],
            )
            .unwrap();
        assert!(store.committed_deletions("connection").is_err());
    }

    #[test]
    fn removing_a_connection_leaves_no_row_behind() {
        let (_root, store) = store();
        store
            .record_observations("connection", &targets(&["a"]), 1)
            .unwrap();
        store
            .replace_deletions(
                "connection",
                &[CommittedDeletion {
                    locator: locator(None, "pack-a"),
                    role: ObjectRole::Pack,
                    byte_length: 1,
                    decided_at_ms: 1,
                    done: false,
                }],
            )
            .unwrap();
        store.set_last_reachable_bytes("connection", 99).unwrap();
        store
            .0
            .execute(
                "INSERT INTO lease_intents(connection_id,locator,kind,job_id,seq,bytes,state,created_at_ms)
                 VALUES('connection','l','cleanup','job',1,x'00','pending',1)",
                [],
            )
            .unwrap();
        store
            .0
            .execute(
                "INSERT INTO delete_requests(connection_id,locator,attempt_id,sent_at_ms,finished)
                 VALUES('connection','l','attempt',1,0)",
                [],
            )
            .unwrap();
        store.forget_connection("connection").unwrap();
        for table in [
            "observations",
            "deletions",
            "lease_intents",
            "delete_requests",
            "cleanup_state",
        ] {
            let rows: i64 = store
                .0
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE connection_id='connection'"),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(rows, 0, "{table} kept a row");
        }
    }

    fn intent(object: &str, kind: LeaseKind, seq: u64) -> LeaseIntent {
        LeaseIntent {
            locator: locator(None, object),
            kind,
            job_id: "job".into(),
            seq,
            bytes: vec![1, 2, 3],
            state: LeaseState::Pending,
            created_at_ms: 1_000,
        }
    }

    /// GC27: the bytes a retry resends come back from the row, not from a
    /// document built again, and confirming moves the row to the locator the
    /// repository answered with.
    #[test]
    fn a_lease_intent_keeps_its_bytes_and_moves_to_the_confirmed_locator() {
        let (_root, store) = store();
        let placed = intent("work-a", LeaseKind::Work, 1);
        store.put_lease_intent("connection", &placed).unwrap();
        assert_eq!(store.lease_intents("connection").unwrap(), vec![placed.clone()]);
        let confirmed = locator(Some("leases"), "leases/work-a");
        store
            .confirm_lease_intent("connection", &placed.locator, &confirmed)
            .unwrap();
        let rows = store.lease_intents("connection").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].locator, confirmed);
        assert_eq!(rows[0].state, LeaseState::Confirmed);
        assert_eq!(rows[0].bytes, placed.bytes);
        // Confirming a row that was never written is a fault, not a new row.
        assert_eq!(
            store
                .confirm_lease_intent("connection", &locator(None, "work-b"), &confirmed)
                .unwrap_err()
                .kind,
            ErrorKind::Corrupt
        );
    }

    /// GC27: one job's renewal and its predecessor coexist, and the same tag
    /// under two kinds does not collide.
    #[test]
    fn two_lease_rows_of_one_job_and_two_kinds_of_one_tag_coexist() {
        let (_root, store) = store();
        store
            .put_lease_intent("connection", &intent("work-a", LeaseKind::Work, 1))
            .unwrap();
        store
            .put_lease_intent("connection", &intent("work-b", LeaseKind::Work, 2))
            .unwrap();
        store
            .put_lease_intent("connection", &intent("deleting-a", LeaseKind::Deleting, 1))
            .unwrap();
        assert_eq!(store.lease_intents("connection").unwrap().len(), 3);
        store
            .set_lease_state("connection", &locator(None, "work-a"), LeaseState::Releasing)
            .unwrap();
        store
            .remove_lease_intent("connection", &locator(None, "work-a"))
            .unwrap();
        let rows = store.lease_intents("connection").unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row.locator.object != "work-a"));
    }

    /// GC31: the row of a request whose answer never arrived stays unfinished,
    /// and a different attempt's answered request does not clear it.
    #[test]
    fn a_delete_request_stays_unfinished_until_its_own_answer_arrives() {
        let (_root, store) = store();
        let pack = locator(None, "pack-a");
        let catalog = locator(None, "catalog-a");
        store
            .record_delete_request("connection", &pack, "attempt-1", 10)
            .unwrap();
        store
            .record_delete_request("connection", &catalog, "attempt-2", 20)
            .unwrap();
        // Sending the same request again keeps the first record.
        store
            .record_delete_request("connection", &pack, "attempt-1", 99)
            .unwrap();
        store
            .finish_delete_request("connection", &catalog, "attempt-2")
            .unwrap();
        let outstanding = store.unfinished_delete_requests("connection", None).unwrap();
        assert_eq!(outstanding.len(), 1);
        assert_eq!(outstanding[0].locator, pack);
        assert_eq!(outstanding[0].sent_at_ms, 10);
        assert!(store
            .unfinished_delete_requests("connection", Some("attempt-2"))
            .unwrap()
            .is_empty());
        store
            .forget_finished_delete_requests("connection", "attempt-1")
            .unwrap();
        assert_eq!(
            store
                .unfinished_delete_requests("connection", Some("attempt-1"))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn the_reachable_total_is_absent_until_a_run_computes_one() {
        let (_root, store) = store();
        assert_eq!(store.last_reachable_bytes("connection").unwrap(), None);
        store.set_last_reachable_bytes("connection", 512).unwrap();
        assert_eq!(store.last_reachable_bytes("connection").unwrap(), Some(512));
        store.set_last_reachable_bytes("connection", 256).unwrap();
        assert_eq!(store.last_reachable_bytes("connection").unwrap(), Some(256));
    }
}
