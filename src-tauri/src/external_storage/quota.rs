//! Process-local waits shared by an account, not by a repository or token.
//! Actual MYBOX counters live separately in `durable_quota`.
use super::contract::{Cancellation, ErrorKind, ProviderError, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct AccountKey {
    provider: String,
    authority: String,
    principal: String,
    pending: bool,
}
impl std::fmt::Debug for AccountKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountKey")
            .field("provider", &self.provider)
            .field("authority", &self.authority)
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}
impl AccountKey {
    /// The API origin comes from validated provider configuration, never a
    /// signed transfer URL. The principal is the stable account identifier.
    pub fn new(provider: &str, api: &url::Url, principal: &str) -> Result<Self> {
        if provider.is_empty() || provider.len() > 64
            || !provider.bytes().all(|byte| byte.is_ascii_lowercase() || byte == b'_' || byte.is_ascii_digit())
            || principal.is_empty() || principal.len() > 1024
            || principal.chars().any(char::is_control)
            || api.host_str().is_none() || !api.username().is_empty()
            || api.password().is_some() || api.query().is_some() || api.fragment().is_some()
            || !matches!(api.scheme(), "https" | "http")
        {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        Ok(Self {
            provider: provider.into(),
            authority: api.origin().ascii_serialization(),
            principal: principal.into(),
            pending: false,
        })
    }

    /// One key per authorization execution. It is never persisted and cannot
    /// collide with an authenticated principal even if its text happens to match.
    pub fn pending(provider: &str, api: &url::Url) -> Result<Self> {
        let mut key = Self::new(provider, api, &uuid::Uuid::new_v4().to_string())?;
        key.pending = true;
        Ok(key)
    }
    pub fn provider(&self) -> &str { &self.provider }
    pub fn authority(&self) -> &str { &self.authority }
    pub fn principal(&self) -> &str { &self.principal }
    pub fn is_pending(&self) -> bool { self.pending }

    pub fn can_resolve_to(&self, account: &Self) -> bool {
        self.pending && !account.pending
            && self.provider == account.provider && self.authority == account.authority
    }
}

const CREDENTIAL_PRINCIPAL_PREFIX: &str = "credential-sha256:";

/// Builds a stable opaque principal from an authentication value. Callers
/// validate the provider and credential before deriving it.
pub(crate) fn credential_principal(provider: &str, credential: &[u8]) -> String {
    let mut hash = Sha256::new();
    hash.update(provider.as_bytes());
    hash.update(b"\0");
    hash.update(credential);
    format!("{CREDENTIAL_PRINCIPAL_PREFIX}{}", hex::encode(hash.finalize()))
}

#[derive(Default)]
struct AccountWait {
    next_allowed: Option<Instant>,
    failures: u32,
    last_error: Option<ErrorKind>,
}

#[derive(Default)]
pub(crate) struct AccountBackoff {
    accounts: Mutex<HashMap<AccountKey, AccountWait>>,
}

fn poisoned<T>(_: T) -> ProviderError { ProviderError::new(ErrorKind::Transient) }

pub(crate) fn bounded_fallback_seconds(failures: u32) -> u64 {
    1u64.checked_shl(failures.saturating_sub(1).min(9))
        .unwrap_or(300).min(300)
}

pub(crate) fn next_delay_seconds(failures: u32, retry_after_seconds: Option<u64>) -> u64 {
    bounded_fallback_seconds(failures).max(retry_after_seconds.unwrap_or(0))
}

impl AccountBackoff {
    pub fn remaining(&self, account: &AccountKey, now: Instant) -> Result<Duration> {
        let accounts = self.accounts.lock().map_err(poisoned)?;
        Ok(accounts.get(account).and_then(|value| value.next_allowed)
            .map(|until| until.saturating_duration_since(now)).unwrap_or_default())
    }

    /// Never holds a mutex across an await. An extension made while sleeping is
    /// observed before dispatch. A control deadline cannot be spent on an hour's wait.
    pub async fn wait(
        &self,
        account: &AccountKey,
        cancel: &Cancellation,
        deadline: Option<Instant>,
    ) -> Result<()> {
        loop {
            cancel.check()?;
            let now = Instant::now();
            if deadline.is_some_and(|deadline| now >= deadline) {
                return Err(ProviderError::new(ErrorKind::Transient));
            }
            let delay = self.remaining(account, now)?;
            if delay.is_zero() { return Ok(()); }
            if deadline.is_some_and(|deadline| delay >= deadline.saturating_duration_since(now)) {
                return Err(ProviderError::new(ErrorKind::RateLimited));
            }
            tokio::select! {
                _ = cancel.cancelled() => return Err(ProviderError::new(ErrorKind::Cancelled)),
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    pub fn failure(
        &self,
        account: &AccountKey,
        kind: ErrorKind,
        retry_after: Option<Duration>,
        now: Instant,
    ) -> Result<()> {
        let mut accounts = self.accounts.lock().map_err(poisoned)?;
        let value = accounts.entry(account.clone()).or_default();
        value.last_error = Some(kind);
        if !matches!(kind, ErrorKind::Transient | ErrorKind::RateLimited | ErrorKind::DailyQuotaExhausted) {
            return Ok(());
        }
        value.failures = value.failures.saturating_add(1);
        let delay = Duration::from_secs(next_delay_seconds(value.failures, None))
            .max(retry_after.unwrap_or_default());
        let until = now.checked_add(delay).ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
        value.next_allowed = Some(value.next_allowed.map_or(until, |previous| previous.max(until)));
        Ok(())
    }

    /// A real minimum request interval is not an error and does not increase
    /// the failure streak. It still cannot shorten a server-imposed pause.
    pub fn defer_for(&self, account: &AccountKey, duration: Duration, now: Instant) -> Result<()> {
        let until = now.checked_add(duration).ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
        let mut accounts = self.accounts.lock().map_err(poisoned)?;
        let value = accounts.entry(account.clone()).or_default();
        value.next_allowed = Some(value.next_allowed.map_or(until, |previous| previous.max(until)));
        Ok(())
    }

    pub fn success(&self, account: &AccountKey) -> Result<()> {
        let mut accounts = self.accounts.lock().map_err(poisoned)?;
        if let Some(value) = accounts.get_mut(account) {
            value.failures = 0;
            value.last_error = None;
            // An earlier in-flight request can succeed after another request
            // has imposed a longer pause. That pause must not be shortened.
        }
        Ok(())
    }

    pub fn resolve_pending(&self, pending: &AccountKey, account: &AccountKey) -> Result<()> {
        if !pending.can_resolve_to(account) {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let mut accounts = self.accounts.lock().map_err(poisoned)?;
        if let Some(source) = accounts.remove(pending) {
            let target = accounts.entry(account.clone()).or_default();
            if source.next_allowed > target.next_allowed {
                target.next_allowed = source.next_allowed;
                target.last_error = source.last_error;
            }
            target.failures = target.failures.max(source.failures);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(principal: &str, endpoint: &str) -> AccountKey {
        AccountKey::new("s3", &url::Url::parse(endpoint).unwrap(), principal).unwrap()
    }

    #[test]
    fn exponential_waits_cap_at_five_minutes_but_do_not_shorten_retry_after() {
        for (count, seconds) in [(1, 1), (2, 2), (9, 256), (10, 300), (u32::MAX, 300)] {
            assert_eq!(next_delay_seconds(count, None), seconds);
            assert_eq!(next_delay_seconds(count, Some(3600)), 3600);
        }
    }

    #[test]
    fn account_keys_share_an_api_authority_but_not_a_principal_or_service() {
        let first = account("a", "https://SYNTHETIC.invalid:443/root-one");
        let second = account("a", "https://synthetic.invalid/root-two");
        assert_eq!(first, second);
        assert_ne!(first, account("b", "https://synthetic.invalid/root-one"));
        assert_ne!(first, account("a", "https://other.invalid/root-one"));
        assert_ne!(first, AccountKey::new("webdav", &url::Url::parse("https://synthetic.invalid").unwrap(), "a").unwrap());
        assert!(AccountKey::new("s3", &url::Url::parse("https://synthetic.invalid/?token=secret").unwrap(), "a").is_err());
        assert!(!format!("{first:?}").contains("principal"));
    }

    #[test]
    fn credential_principals_are_stable_separated_and_opaque() {
        let first = credential_principal("mybox", b"synthetic-token-a");
        assert_eq!(first, credential_principal("mybox", b"synthetic-token-a"));
        assert_ne!(first, credential_principal("mybox", b"synthetic-token-b"));
        assert_ne!(first, credential_principal("s3", b"synthetic-token-a"));
        assert!(!first.contains("synthetic-token-a"));
        assert_eq!(first.len(), CREDENTIAL_PRINCIPAL_PREFIX.len() + 64);
    }

    #[test]
    fn credential_accounts_share_only_the_same_provider_authority_and_credential() {
        let principal = credential_principal("s3", b"synthetic-access-key");
        let first = account(&principal, "https://synthetic.invalid/root-one");
        assert_eq!(first, account(&principal, "https://synthetic.invalid/root-two"));
        assert_ne!(
            first,
            account(
                &credential_principal("s3", b"different-access-key"),
                "https://synthetic.invalid/root-one",
            )
        );
        assert_ne!(
            first,
            account(&principal, "https://other.invalid/root-one")
        );
    }

    #[test]
    fn concurrent_shorter_results_and_success_never_erase_a_longer_pause() {
        let state = AccountBackoff::default();
        let account = account("a", "https://synthetic.invalid");
        let now = Instant::now();
        state.failure(&account, ErrorKind::RateLimited, Some(Duration::from_secs(3600)), now).unwrap();
        state.failure(&account, ErrorKind::Transient, None, now).unwrap();
        state.success(&account).unwrap();
        assert_eq!(state.remaining(&account, now).unwrap(), Duration::from_secs(3600));
        let state = state.accounts.lock().unwrap();
        assert_eq!(state[&account].failures, 0);
        assert_eq!(state[&account].last_error, None);
    }

    #[test]
    fn pending_execution_merges_without_refunding_the_authenticated_account_wait() {
        let state = AccountBackoff::default();
        let api = url::Url::parse("https://synthetic.invalid").unwrap();
        let pending = AccountKey::pending("s3", &api).unwrap();
        let other_pending = AccountKey::pending("s3", &api).unwrap();
        let authenticated = AccountKey::new("s3", &api, "a").unwrap();
        let now = Instant::now();
        state.failure(&pending, ErrorKind::Transient, None, now).unwrap();
        state.failure(&authenticated, ErrorKind::RateLimited, Some(Duration::from_secs(3600)), now).unwrap();
        state.resolve_pending(&pending, &authenticated).unwrap();
        assert_eq!(state.remaining(&authenticated, now).unwrap(), Duration::from_secs(3600));
        assert_eq!(state.remaining(&other_pending, now).unwrap(), Duration::ZERO);
        assert_eq!(state.remaining(&pending, now).unwrap(), Duration::ZERO);
        assert!(state.resolve_pending(&authenticated, &other_pending).is_err());
    }

    #[test]
    fn waiting_is_cancellable_without_holding_the_account_mutex() {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        runtime.block_on(async {
            let state = AccountBackoff::default();
            let account = account("a", "https://synthetic.invalid");
            state.failure(&account, ErrorKind::RateLimited, Some(Duration::from_secs(3600)), Instant::now()).unwrap();
            let cancel = Cancellation::default();
            let waiting = state.wait(&account, &cancel, None);
            tokio::pin!(waiting);
            assert!(futures::poll!(&mut waiting).is_pending());
            state.success(&account).unwrap();
            cancel.cancel();
            let error = tokio::time::timeout(Duration::from_millis(100), waiting).await.unwrap().unwrap_err();
            assert_eq!(error.kind, ErrorKind::Cancelled);
        });
    }
}
