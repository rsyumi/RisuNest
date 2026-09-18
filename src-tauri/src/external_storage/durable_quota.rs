//! Actual MYBOX consumption only, outside the library PDS so restore cannot
//! refund requests. Independent handles reserve atomically in one SQLite transaction.
use super::{
    contract::{ErrorKind, ProviderError, ProviderFuture, Result},
    http::MyboxRequestBudget,
    quota::AccountKey,
    quota_profiles::{next_daily_reset_ms, MyboxCharge, MyboxCounter, MyboxCounterSummary, MyboxPlan},
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use std::{path::PathBuf, sync::{Arc, Mutex}, time::Duration};

#[derive(Clone)]
pub(crate) struct MyboxBudget {
    path: Arc<PathBuf>,
    connection: Arc<Mutex<Option<Connection>>>,
}
fn storage_error(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}
fn corrupt() -> ProviderError { ProviderError::new(ErrorKind::Corrupt) }

#[derive(Clone, Copy)]
struct AccountCounters {
    now_ms: u64,
    download_reset_at_ms: u64,
    downloads_used: u64,
}

fn require_mybox(account: &AccountKey, now_ms: u64) -> Result<()> {
    if account.provider() != "mybox" || account.is_pending() {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    // SQLite timestamps are signed. Reject impossible input rather than
    // wrapping a clock or producing a reset earlier than the reservation.
    if now_ms > (i64::MAX as u64).saturating_sub(86_400_000) { return Err(corrupt()); }
    Ok(())
}

fn account_counters(db: &Connection, account: &AccountKey, now_ms: u64) -> Result<AccountCounters> {
    let previous: Option<(i64, i64, i64)> = db.query_row(
        "SELECT last_seen_ms,download_reset_at_ms,downloads_used FROM mybox_accounts WHERE authority=?1 AND principal=?2",
        params![account.authority(), account.principal()],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional().map_err(storage_error)?;
    let previous = previous
        .map(|(last_seen, reset, used)| {
            Ok((
                u64::try_from(last_seen).map_err(|_| corrupt())?,
                u64::try_from(reset).map_err(|_| corrupt())?,
                u64::try_from(used).map_err(|_| corrupt())?,
            ))
        })
        .transpose()?;
    let (last_seen, reset, used) = previous.unwrap_or((now_ms, next_daily_reset_ms(now_ms), 0));
    let effective_now = now_ms.max(last_seen);
    require_mybox(account, effective_now)?;
    if reset == 0 || reset > i64::MAX as u64 { return Err(corrupt()); }
    Ok(AccountCounters {
        now_ms: effective_now,
        download_reset_at_ms: if effective_now >= reset { next_daily_reset_ms(effective_now) } else { reset },
        downloads_used: if effective_now >= reset { 0 } else { used },
    })
}

fn minute_usage(db: &Connection, account: &AccountKey, counter: MyboxCounter, now_ms: u64) -> Result<(u64, Option<u64>)> {
    let cutoff = i64::try_from(now_ms).map_err(|_| corrupt())? - 60_000;
    let (used, oldest): (i64, Option<i64>) = db.query_row(
        "SELECT COUNT(*),MIN(issued_at_ms) FROM mybox_minute_requests WHERE authority=?1 AND principal=?2 AND counter=?3 AND issued_at_ms>?4",
        params![account.authority(), account.principal(), counter.id(), cutoff],
        |row| Ok((row.get(0)?, row.get(1)?)),
    ).map_err(storage_error)?;
    Ok((
        u64::try_from(used).map_err(|_| corrupt())?,
        oldest
            .map(|value| u64::try_from(value).map_err(|_| corrupt()))
            .transpose()?,
    ))
}

impl MyboxBudget {
    /// Lazy: a non-MYBOX connection never opens or writes this database.
    pub fn new(path: PathBuf) -> Self {
        Self { path: Arc::new(path), connection: Arc::new(Mutex::new(None)) }
    }

    fn with_db<T>(&self, operation: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut slot = self.connection.lock().map_err(storage_error)?;
        if slot.is_none() {
            let parent = self.path.parent().ok_or_else(corrupt)?;
            let metadata = std::fs::symlink_metadata(parent).map_err(storage_error)?;
            if !metadata.is_dir() || crate::trust_boundary::is_link_like(&metadata) { return Err(corrupt()); }
            if self.path.exists() { crate::trust_boundary::open_regular_source(self.path.as_ref()).map_err(storage_error)?; }
            let db = Connection::open(self.path.as_ref()).map_err(storage_error)?;
            db.busy_timeout(Duration::from_secs(5)).map_err(storage_error)?;
            db.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;
                CREATE TABLE IF NOT EXISTS mybox_accounts(
                    authority TEXT NOT NULL, principal TEXT NOT NULL,
                    last_seen_ms INTEGER NOT NULL CHECK(last_seen_ms>=0),
                    download_reset_at_ms INTEGER NOT NULL CHECK(download_reset_at_ms>0),
                    downloads_used INTEGER NOT NULL CHECK(downloads_used>=0),
                    PRIMARY KEY(authority,principal));
                CREATE TABLE IF NOT EXISTS mybox_minute_requests(
                    authority TEXT NOT NULL, principal TEXT NOT NULL,
                    counter TEXT NOT NULL CHECK(counter IN (
                        'mybox-metadata-minute','mybox-list-minute','mybox-folder-minute',
                        'mybox-upload-url-minute','mybox-download-url-minute','mybox-delete-minute')),
                    issued_at_ms INTEGER NOT NULL CHECK(issued_at_ms>=0),
                    FOREIGN KEY(authority,principal) REFERENCES mybox_accounts(authority,principal));
                CREATE INDEX IF NOT EXISTS mybox_minute_scope ON mybox_minute_requests(authority,principal,counter,issued_at_ms);"
            ).map_err(storage_error)?;
            *slot = Some(db);
        }
        operation(slot.as_mut().ok_or_else(corrupt)?)
    }

    pub fn reserve(&self, account: &AccountKey, charge: &MyboxCharge, now_ms: u64) -> Result<()> {
        require_mybox(account, now_ms)?;
        if charge.counters.is_empty() || charge.counters.len() > 2
            || charge.counters.iter().enumerate().any(|(index, counter)| charge.counters[..index].contains(counter))
        { return Err(corrupt()); }
        self.with_db(|db| {
            let tx = db.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage_error)?;
            let mut state = account_counters(&tx, account, now_ms)?;
            let mut denied: Option<ProviderError> = None;
            for counter in &charge.counters {
                let (used, until, kind) = if *counter == MyboxCounter::DownloadDay {
                    (state.downloads_used, state.download_reset_at_ms, ErrorKind::DailyQuotaExhausted)
                } else {
                    let (used, oldest) = minute_usage(&tx, account, *counter, state.now_ms)?;
                    (used, oldest.unwrap_or(state.now_ms).saturating_add(60_000), ErrorKind::RateLimited)
                };
                if used >= counter.limit(charge.plan) {
                    let error = ProviderError { kind, http_status: None, retry_at_ms: Some(until) };
                    if denied.as_ref().is_none_or(|previous| previous.retry_at_ms < error.retry_at_ms) {
                        denied = Some(error);
                    }
                }
            }
            if let Some(error) = denied { return Err(error); }
            if charge.counters.contains(&MyboxCounter::DownloadDay) {
                state.downloads_used = state.downloads_used.checked_add(1).ok_or_else(corrupt)?;
            }
            tx.execute(
                "INSERT INTO mybox_accounts VALUES(?1,?2,?3,?4,?5) ON CONFLICT(authority,principal) DO UPDATE SET last_seen_ms=excluded.last_seen_ms,download_reset_at_ms=excluded.download_reset_at_ms,downloads_used=excluded.downloads_used",
                params![
                    account.authority(),
                    account.principal(),
                    i64::try_from(state.now_ms).map_err(|_| corrupt())?,
                    i64::try_from(state.download_reset_at_ms).map_err(|_| corrupt())?,
                    i64::try_from(state.downloads_used).map_err(|_| corrupt())?,
                ],
            ).map_err(storage_error)?;
            // A true rolling minute, not a window boundary at which a second
            // full allowance could immediately follow the first one.
            tx.execute(
                "DELETE FROM mybox_minute_requests WHERE authority=?1 AND principal=?2 AND issued_at_ms<=?3",
                params![account.authority(), account.principal(), i64::try_from(state.now_ms).map_err(|_| corrupt())? - 60_000],
            ).map_err(storage_error)?;
            for counter in &charge.counters {
                if *counter != MyboxCounter::DownloadDay {
                    tx.execute("INSERT INTO mybox_minute_requests VALUES(?1,?2,?3,?4)",
                        params![
                            account.authority(),
                            account.principal(),
                            counter.id(),
                            i64::try_from(state.now_ms).map_err(|_| corrupt())?,
                        ])
                        .map_err(storage_error)?;
                }
            }
            tx.commit().map_err(storage_error)
        })
    }

    pub fn snapshot(&self, account: &AccountKey, plan: MyboxPlan, now_ms: u64) -> Result<Vec<MyboxCounterSummary>> {
        require_mybox(account, now_ms)?;
        self.with_db(|db| {
            let tx = db.transaction().map_err(storage_error)?;
            let state = account_counters(&tx, account, now_ms)?;
            let mut summaries = Vec::with_capacity(MyboxCounter::ALL.len());
            for counter in MyboxCounter::ALL {
                let (used, reset_at_ms) = if counter == MyboxCounter::DownloadDay {
                    (state.downloads_used, Some(state.download_reset_at_ms))
                } else {
                    let (used, oldest) = minute_usage(&tx, account, counter, state.now_ms)?;
                    (used, oldest.map(|oldest| oldest.saturating_add(60_000)))
                };
                summaries.push(MyboxCounterSummary { id: counter.id().into(), limit: counter.limit(plan), used, reset_at_ms, local_estimate: true });
            }
            tx.commit().map_err(storage_error)?;
            Ok(summaries)
        })
    }
}

impl MyboxRequestBudget for MyboxBudget {
    fn reserve_mybox<'a>(&'a self, account: &'a AccountKey, charge: &'a MyboxCharge, now_ms: u64) -> ProviderFuture<'a, ()> {
        let owner = self.clone();
        let account = account.clone();
        let charge = charge.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || owner.reserve(&account, &charge, now_ms))
                .await.map_err(storage_error)?
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account() -> AccountKey {
        AccountKey::new("mybox", &url::Url::parse("https://synthetic.invalid").unwrap(), "synthetic-account").unwrap()
    }
    fn charge(counter: MyboxCounter) -> MyboxCharge {
        MyboxCharge { plan: MyboxPlan::Plan30gb, counters: vec![counter] }
    }
    fn seed_last_download(budget: &MyboxBudget, account: &AccountKey) {
        budget.with_db(|db| {
            db.execute("INSERT INTO mybox_accounts VALUES(?1,?2,10,?3,499)",
                params![account.authority(), account.principal(), i64::try_from(next_daily_reset_ms(10)).unwrap()])
                .map_err(storage_error)?;
            Ok(())
        }).unwrap();
    }

    #[test]
    fn concurrent_handles_and_reopen_cannot_refund_a_reserved_download() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mybox.sqlite");
        let account = account();
        let first = MyboxBudget::new(path.clone());
        seed_last_download(&first, &account);
        let second = MyboxBudget::new(path.clone());
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let jobs: Vec<_> = [first, second].into_iter().map(|budget| {
            let barrier = barrier.clone();
            let account = account.clone();
            std::thread::spawn(move || {
                barrier.wait();
                budget.reserve(&account, &charge(MyboxCounter::DownloadDay), 11)
            })
        }).collect();
        let results: Vec<_> = jobs.into_iter().map(|job| job.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|value| value.is_ok()).count(), 1);
        assert_eq!(results.iter().filter_map(|value| value.as_ref().err()).next().unwrap().kind, ErrorKind::DailyQuotaExhausted);
        let reopened = MyboxBudget::new(path);
        assert_eq!(reopened.reserve(&account, &charge(MyboxCounter::DownloadDay), 12).unwrap_err().kind, ErrorKind::DailyQuotaExhausted);
        assert_eq!(reopened.snapshot(&account, MyboxPlan::Plan80gb, 12).unwrap()[0].used, 500);
        reopened.reserve(&account, &charge(MyboxCounter::DownloadDay), next_daily_reset_ms(12)).unwrap();
    }

    #[test]
    fn a_rolling_minute_and_clock_rollback_do_not_refund_requests() {
        let directory = tempfile::tempdir().unwrap();
        let budget = MyboxBudget::new(directory.path().join("mybox.sqlite"));
        let account = account();
        for now in 10..20 { budget.reserve(&account, &charge(MyboxCounter::ListMinute), now).unwrap(); }
        for now in [1, 20, 60_009] {
            let error = budget.reserve(&account, &charge(MyboxCounter::ListMinute), now).unwrap_err();
            assert_eq!(error.kind, ErrorKind::RateLimited);
            assert_eq!(error.retry_at_ms, Some(60_010));
        }
        budget.reserve(&account, &charge(MyboxCounter::ListMinute), 60_010).unwrap();
        assert!(budget.reserve(&account, &charge(MyboxCounter::ListMinute), 60_010).is_err());
    }

    #[test]
    fn a_denied_minute_does_not_partially_consume_the_daily_allowance() {
        let directory = tempfile::tempdir().unwrap();
        let budget = MyboxBudget::new(directory.path().join("mybox.sqlite"));
        let account = account();
        for _ in 0..60 { budget.reserve(&account, &charge(MyboxCounter::DownloadUrlMinute), 10).unwrap(); }
        let both = MyboxCharge { plan: MyboxPlan::Plan30gb, counters: vec![MyboxCounter::DownloadUrlMinute, MyboxCounter::DownloadDay] };
        assert!(budget.reserve(&account, &both, 11).is_err());
        let summaries = budget.snapshot(&account, MyboxPlan::Plan30gb, 11).unwrap();
        assert_eq!(summaries[0].used, 0);
        assert!(summaries.iter().all(|value| value.local_estimate));
        assert!(serde_json::to_value(&summaries).unwrap()[0].get("remaining").is_none());
    }

    #[test]
    fn persistence_failure_never_grants_a_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let budget = MyboxBudget::new(directory.path().join("mybox.sqlite"));
        let account = account();
        budget.with_db(|db| db.execute_batch("PRAGMA query_only=ON").map_err(storage_error)).unwrap();
        assert!(budget.reserve(&account, &charge(MyboxCounter::DownloadDay), 1).is_err());
        budget.with_db(|db| db.execute_batch("PRAGMA query_only=OFF").map_err(storage_error)).unwrap();
        budget.reserve(&account, &charge(MyboxCounter::DownloadDay), 1).unwrap();
        assert_eq!(budget.snapshot(&account, MyboxPlan::Plan30gb, 1).unwrap()[0].used, 1);
    }
}
