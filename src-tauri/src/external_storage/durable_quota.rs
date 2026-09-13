//! Account quotas live outside the library PDS, so restore cannot refund calls.
//! SQLite supplies process-safe atomic reservation and crash durability.
use super::{
    contract::*,
    http::RequestBudget,
    quota::{Bucket, QuotaLedger},
};
use rusqlite::{Connection, TransactionBehavior};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

pub(crate) struct DurableBudget(Arc<Mutex<Connection>>);
fn storage_error(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}
impl DurableBudget {
    /// The native account owner supplies an app-private path, never a library backup path.
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path).map_err(storage_error)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(storage_error)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS external_request_budget (id INTEGER PRIMARY KEY CHECK(id=1), schema_version INTEGER NOT NULL CHECK(schema_version=1), ledger TEXT NOT NULL);").map_err(storage_error)?;
        connection
            .execute(
                "INSERT OR IGNORE INTO external_request_budget VALUES (1, 1, ?1)",
                [serde_json::to_string(&QuotaLedger::default()).map_err(storage_error)?],
            )
            .map_err(storage_error)?;
        let result = Self(Arc::new(Mutex::new(connection)));
        result.update(|_| Ok(()))?;
        Ok(result)
    }
    fn update(&self, operation: impl FnOnce(&mut QuotaLedger) -> Result<()>) -> Result<()> {
        let mut connection = self.0.lock().map_err(storage_error)?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(storage_error)?;
        let (schema, bytes): (u32, String) = transaction
            .query_row(
                "SELECT schema_version, ledger FROM external_request_budget WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(storage_error)?;
        if schema != 1 || bytes.len() > 4 * 1024 * 1024 {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let mut ledger: QuotaLedger =
            serde_json::from_str(&bytes).map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
        operation(&mut ledger)?;
        let encoded = serde_json::to_string(&ledger).map_err(storage_error)?;
        if encoded.len() > 4 * 1024 * 1024 {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        transaction
            .execute(
                "UPDATE external_request_budget SET ledger=?1 WHERE id=1",
                [encoded],
            )
            .map_err(storage_error)?;
        transaction.commit().map_err(storage_error)
    }
    pub fn configure(&self, account: &str, name: &str, bucket: Bucket) -> Result<()> {
        self.update(|ledger| {
            ledger.configure(account, name, bucket);
            Ok(())
        })
    }
}
impl RequestBudget for DurableBudget {
    fn reserve<'a>(&'a self, costs: &'a [RequestCost], now_ms: u64) -> ProviderFuture<'a, ()> {
        let owner = Self(Arc::clone(&self.0));
        let costs = costs.to_vec();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                owner.update(|ledger| ledger.reserve(&costs, now_ms))
            })
            .await
            .map_err(storage_error)?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn cost() -> RequestCost {
        RequestCost {
            bucket: "download".into(),
            shared_account: "synthetic-account".into(),
            units: 1,
            reset: QuotaReset::Unknown,
        }
    }
    fn bucket() -> Bucket {
        Bucket {
            limit: 1,
            used: 0,
            reset: QuotaReset::Unknown,
            blocked_until_ms: None,
            last_reset_ms: None,
        }
    }
    #[test]
    fn concurrent_handles_and_reopen_cannot_refund_a_reserved_request() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("account-quota.sqlite");
        let a = DurableBudget::open(&path).unwrap();
        a.configure("synthetic-account", "download", bucket())
            .unwrap();
        let b = DurableBudget::open(&path).unwrap();
        let gate = Arc::new(std::sync::Barrier::new(2));
        let jobs: Vec<_> = [a, b]
            .into_iter()
            .map(|budget| {
                let gate = gate.clone();
                std::thread::spawn(move || {
                    gate.wait();
                    budget.update(|ledger| ledger.reserve(&[cost()], 10))
                })
            })
            .collect();
        let results: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter_map(|r| r.as_ref().err())
                .next()
                .unwrap()
                .kind,
            ErrorKind::DailyQuotaExhausted
        );
        let reopened = DurableBudget::open(&path).unwrap();
        reopened
            .configure("synthetic-account", "download", bucket())
            .unwrap();
        assert_eq!(
            reopened
                .update(|ledger| ledger.reserve(&[cost()], 20))
                .unwrap_err()
                .kind,
            ErrorKind::DailyQuotaExhausted
        );
    }
    #[test]
    fn failed_persistence_does_not_grant_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let budget = DurableBudget::open(&directory.path().join("quota.sqlite")).unwrap();
        budget
            .configure("synthetic-account", "download", bucket())
            .unwrap();
        budget
            .0
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=ON")
            .unwrap();
        assert!(budget
            .update(|ledger| ledger.reserve(&[cost()], 1))
            .is_err());
        budget
            .0
            .lock()
            .unwrap()
            .execute_batch("PRAGMA query_only=OFF")
            .unwrap();
        assert!(budget.update(|ledger| ledger.reserve(&[cost()], 1)).is_ok());
    }
}
