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
    pub fn reserve(&mut self, costs: &[RequestCost], now_ms: u64) -> Result<()> {
        let mut pending = self.clone();
        for cost in costs {
            let Some(bucket) = pending
                .buckets
                .get_mut(&key(&cost.shared_account, &cost.bucket))
            else {
                continue;
            };
            if let QuotaReset::At { unix_ms } = bucket.reset {
                if now_ms >= unix_ms && bucket.last_reset_ms.is_none_or(|last| unix_ms > last) {
                    // Do not invent tomorrow's reset. Await provider's next reset
                    // metadata while allowing this newly opened window once.
                    bucket.used = 0;
                    bucket.last_reset_ms = Some(unix_ms);
                    bucket.reset = QuotaReset::Unknown;
                    bucket.blocked_until_ms = None;
                }
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
}
