//! Durable account budget, independent of library restores. The owner persists
//! the returned state before dispatching each HTTP/SDK subrequest.
use super::contract::{ErrorKind, ProviderError, QuotaReset, RequestCost, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Bucket {
    pub limit: u64,
    pub used: u64,
    pub reset: QuotaReset,
    pub blocked_until_ms: Option<u64>,
    pub last_reset_ms: Option<u64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuotaLedger {
    buckets: BTreeMap<String, Bucket>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BucketSnapshot {
    pub limit: Option<u64>,
    pub used: u64,
    pub reset: QuotaReset,
    pub blocked_until_ms: Option<u64>,
}
fn key(account: &str, bucket: &str) -> String {
    format!("{}:{account}{bucket}", account.len())
}
impl QuotaLedger {
    pub fn configure(&mut self, account: &str, bucket: &str, value: Bucket) {
        self.buckets
            .entry(key(account, bucket))
            .and_modify(|existing| {
                existing.limit = value.limit;
                // Reopening or rediscovering the same window cannot refund calls.
                if let QuotaReset::At { unix_ms } = value.reset {
                    if existing.last_reset_ms.is_none_or(|last| unix_ms > last) {
                        existing.reset = QuotaReset::At { unix_ms };
                    }
                }
            })
            .or_insert(value);
    }
    fn refresh(bucket: &mut Bucket, observed: &QuotaReset, now_ms: u64) {
        match observed {
            QuotaReset::At { unix_ms }
                if bucket.last_reset_ms.is_none_or(|last| *unix_ms > last) =>
            {
                if !matches!(bucket.reset, QuotaReset::At { unix_ms: current } if current >= *unix_ms)
                {
                    bucket.reset = observed.clone();
                }
            }
            QuotaReset::Rolling { window_ms } if *window_ms > 0 => {
                if !matches!(bucket.reset, QuotaReset::Rolling { window_ms: current } if current == *window_ms)
                {
                    bucket.reset = observed.clone();
                }
                let opened_at = bucket.last_reset_ms.get_or_insert(now_ms);
                if now_ms >= opened_at.saturating_add(*window_ms) {
                    bucket.used = 0;
                    bucket.last_reset_ms = Some(now_ms);
                }
            }
            _ => {}
        }
        if let QuotaReset::At { unix_ms } = bucket.reset {
            if now_ms >= unix_ms && bucket.last_reset_ms.is_none_or(|last| unix_ms > last) {
                bucket.used = 0;
                bucket.last_reset_ms = Some(unix_ms);
                bucket.reset = QuotaReset::Unknown;
            }
        }
    }
    pub fn reserve(&mut self, costs: &[RequestCost], now_ms: u64) -> Result<()> {
        let mut pending = self.clone();
        for cost in costs {
            let bucket = pending
                .buckets
                .entry(key(&cost.shared_account, &cost.bucket))
                .or_insert_with(|| Bucket {
                    limit: u64::MAX,
                    used: 0,
                    reset: cost.reset.clone(),
                    blocked_until_ms: None,
                    last_reset_ms: matches!(cost.reset, QuotaReset::Rolling { .. })
                        .then_some(now_ms),
                });
            Self::refresh(bucket, &cost.reset, now_ms);
            if bucket.blocked_until_ms.is_some_and(|until| until <= now_ms) {
                bucket.blocked_until_ms = None;
            }
            if let Some(until) = bucket.blocked_until_ms.filter(|until| *until > now_ms) {
                return Err(ProviderError {
                    kind: ErrorKind::RateLimited,
                    http_status: None,
                    retry_at_ms: Some(until),
                });
            }
            let used = bucket
                .used
                .checked_add(cost.units)
                .ok_or_else(|| ProviderError::new(ErrorKind::DailyQuotaExhausted))?;
            if used > bucket.limit {
                return Err(ProviderError {
                    kind: ErrorKind::DailyQuotaExhausted,
                    http_status: None,
                    retry_at_ms: match bucket.reset {
                        QuotaReset::At { unix_ms } => Some(unix_ms),
                        _ => None,
                    },
                });
            }
            bucket.used = used;
        }
        *self = pending;
        Ok(())
    }
    pub fn used(&self, account: &str, bucket: &str) -> Option<u64> {
        self.buckets.get(&key(account, bucket)).map(|b| b.used)
    }
    pub fn block(&mut self, costs: &[RequestCost], until_ms: u64) -> Result<()> {
        let mut pending = self.clone();
        for cost in costs {
            let bucket = pending
                .buckets
                .get_mut(&key(&cost.shared_account, &cost.bucket))
                .ok_or_else(|| ProviderError::new(ErrorKind::DailyQuotaExhausted))?;
            bucket.blocked_until_ms =
                Some(bucket.blocked_until_ms.unwrap_or_default().max(until_ms));
        }
        *self = pending;
        Ok(())
    }
    pub fn snapshot(&self, account: &str, bucket: &str) -> Option<BucketSnapshot> {
        self.buckets
            .get(&key(account, bucket))
            .map(|value| BucketSnapshot {
                limit: (value.limit != u64::MAX).then_some(value.limit),
                used: value.used,
                reset: value.reset.clone(),
                blocked_until_ms: value.blocked_until_ms,
            })
    }
}
