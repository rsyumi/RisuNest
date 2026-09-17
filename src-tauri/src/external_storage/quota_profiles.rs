//! Provider quota profiles turn request-cost metadata into durable account
//! buckets. Unknown service capacity is still counted, but is never presented
//! as a known provider limit.
use super::{
    contract::{
        ErrorKind, Provider, ProviderError, ProviderOperation, QuotaReset, RepositoryHandle,
        RequestCost, Result,
    },
    durable_quota::DurableBudget,
    quota::Bucket,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const OPERATIONS: [ProviderOperation; 14] = [
    ProviderOperation::Metadata,
    ProviderOperation::List,
    ProviderOperation::DownloadUrl,
    ProviderOperation::Get,
    ProviderOperation::Range,
    ProviderOperation::Create,
    ProviderOperation::UploadSession,
    ProviderOperation::UploadChunk,
    ProviderOperation::CompleteUpload,
    ProviderOperation::ReconcileUpload,
    ProviderOperation::CompareExchangeHead,
    ProviderOperation::ReplaceHead,
    ProviderOperation::Delete,
    ProviderOperation::Authenticate,
];

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct QuotaLimitOverride {
    pub bucket: String,
    pub limit: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct QuotaBucketSummary {
    pub bucket: String,
    pub limit: Option<u64>,
    pub used: u64,
    pub reset: QuotaReset,
    pub blocked_until_ms: Option<u64>,
    /// Consumption is local to this installation. Other clients sharing the
    /// provider account can consume the same remote allowance.
    pub local_estimate: bool,
}

/// Configure every bucket reported by a provider using the handle's resolved
/// account identities. Limits not documented as stable stay locally unbounded,
/// while reservations continue to count their units durably.
pub(crate) fn configure_connection_budget(
    budget: &DurableBudget,
    provider_id: &str,
    profile: Option<&str>,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    overrides: &[QuotaLimitOverride],
    now_ms: u64,
) -> Result<Vec<QuotaBucketSummary>> {
    let mut costs = Vec::new();
    for operation in OPERATIONS {
        costs.extend(provider.request_cost(repository, operation)?);
    }
    configure_costs(budget, provider_id, profile, &costs, overrides, now_ms)
}

/// The runtime can configure an OAuth token request before a repository handle
/// exists by supplying the config-derived account cost. The same override and
/// snapshot rules apply when the actual handle is configured after open.
pub(crate) fn configure_initial_auth_budget(
    budget: &DurableBudget,
    provider_id: &str,
    profile: Option<&str>,
    auth_costs: &[RequestCost],
    overrides: &[QuotaLimitOverride],
    now_ms: u64,
) -> Result<Vec<QuotaBucketSummary>> {
    configure_costs(budget, provider_id, profile, auth_costs, overrides, now_ms)
}

fn configure_costs(
    budget: &DurableBudget,
    provider_id: &str,
    profile: Option<&str>,
    costs: &[RequestCost],
    overrides: &[QuotaLimitOverride],
    now_ms: u64,
) -> Result<Vec<QuotaBucketSummary>> {
    let invalid = || ProviderError::new(ErrorKind::Unsupported);
    let mut override_limits = BTreeMap::new();
    for value in overrides {
        if value.bucket.is_empty()
            || value.bucket.len() > 128
            || override_limits
                .insert(value.bucket.clone(), value.limit)
                .is_some()
        {
            return Err(invalid());
        }
    }

    let mut discovered = BTreeMap::<(String, String), QuotaReset>::new();
    for cost in costs {
        if cost.bucket.is_empty() || cost.shared_account.is_empty() {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let key = (cost.shared_account.clone(), cost.bucket.clone());
        if let Some(existing) = discovered.get(&key) {
            if !compatible_reset(existing, &cost.reset) {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
        } else {
            discovered.insert(key, cost.reset.clone());
        }
    }
    let discovered_names: BTreeSet<&str> = discovered
        .keys()
        .map(|(_, bucket)| bucket.as_str())
        .collect();
    if override_limits
        .keys()
        .any(|bucket| !discovered_names.contains(bucket.as_str()))
    {
        return Err(invalid());
    }

    let configured: Vec<_> = discovered
        .iter()
        .map(|((account, bucket), reset)| {
            let limit = override_limits
                .get(bucket)
                .copied()
                .or_else(|| documented_limit(provider_id, profile, bucket))
                .unwrap_or(u64::MAX);
            (
                account.clone(),
                bucket.clone(),
                Bucket {
                    limit,
                    used: 0,
                    reset: reset.clone(),
                    blocked_until_ms: None,
                    last_reset_ms: matches!(reset, QuotaReset::Rolling { .. }).then_some(now_ms),
                },
            )
        })
        .collect();
    budget.configure_many(&configured)?;

    discovered
        .into_iter()
        .map(|((account, bucket), _)| {
            let value = budget
                .snapshot(&account, &bucket)?
                .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
            Ok(QuotaBucketSummary {
                bucket,
                limit: value.limit,
                used: value.used,
                reset: value.reset,
                blocked_until_ms: value.blocked_until_ms,
                local_estimate: true,
            })
        })
        .collect()
}

fn compatible_reset(left: &QuotaReset, right: &QuotaReset) -> bool {
    match (left, right) {
        (QuotaReset::Unknown, QuotaReset::Unknown) => true,
        (QuotaReset::At { unix_ms: left }, QuotaReset::At { unix_ms: right }) => left == right,
        (QuotaReset::Rolling { window_ms: left }, QuotaReset::Rolling { window_ms: right }) => {
            left == right
        }
        _ => false,
    }
}

fn documented_limit(provider_id: &str, profile: Option<&str>, bucket: &str) -> Option<u64> {
    match (provider_id, profile, bucket) {
        ("github_releases", _, "rest-primary") => Some(5_000),
        ("github_releases", _, "rest-endpoint-points") => Some(900),
        ("github_releases", _, "content-creation-minute") => Some(80),
        ("github_releases", _, "content-creation-hour") => Some(500),
        ("s3", Some("b2"), "requests_per_second") => Some(500),
        ("s3", Some("r2"), "head_key_write") => Some(1),
        ("mybox", profile, bucket) => mybox_limit(profile.unwrap_or("plan30gb"), bucket),
        _ => None,
    }
}

fn mybox_limit(profile: &str, bucket: &str) -> Option<u64> {
    let (downloads, search, general) = match profile {
        "plan30gb" => (500, 10, 60),
        "plan80gb" => (1_000, 10, 60),
        "plan180gb" => (1_000, 30, 240),
        "plan2tb" => (2_000, 30, 240),
        "plan5tb" => (5_000, 30, 240),
        "plan10tb" => (20_000, 30, 240),
        "plan20tb" => (50_000, 30, 240),
        _ => return None,
    };
    match bucket {
        "mybox-download-day" => Some(downloads),
        "mybox-list-minute" => Some(search),
        // Deletion is documented as its own per-minute allowance; on every plan
        // it happens to carry the same number as the remaining APIs.
        "mybox-metadata-minute"
        | "mybox-folder-minute"
        | "mybox-upload-url-minute"
        | "mybox-download-url-minute"
        | "mybox-delete-minute" => Some(general),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cost(account: &str, bucket: &str, reset: QuotaReset) -> RequestCost {
        RequestCost {
            bucket: bucket.into(),
            shared_account: account.into(),
            units: 1,
            reset,
        }
    }

    #[test]
    fn documented_defaults_and_unknown_capacity_are_reported_distinctly() {
        let directory = tempfile::tempdir().unwrap();
        let budget = DurableBudget::open(&directory.path().join("quota.sqlite")).unwrap();
        let summaries = configure_costs(
            &budget,
            "github_releases",
            None,
            &[
                cost(
                    "account",
                    "rest-primary",
                    QuotaReset::Rolling {
                        window_ms: 3_600_000,
                    },
                ),
                cost("account", "asset-body", QuotaReset::Unknown),
            ],
            &[],
            10,
        )
        .unwrap();
        assert_eq!(summaries[0].bucket, "asset-body");
        assert_eq!(summaries[0].limit, None);
        assert_eq!(summaries[1].limit, Some(5_000));
        assert!(summaries.iter().all(|summary| summary.local_estimate));
        let unknown = [RequestCost {
            bucket: "asset-body".into(),
            shared_account: "account".into(),
            units: u64::MAX - 1,
            reset: QuotaReset::Unknown,
        }];
        budget.reserve_for_tests(&unknown, 11).unwrap();
        assert_eq!(
            budget
                .snapshot("account", "asset-body")
                .unwrap()
                .unwrap()
                .used,
            u64::MAX - 1
        );
    }

    #[test]
    fn mybox_plan_defaults_can_be_overridden_without_refunding_usage() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quota.sqlite");
        let costs = [cost(
            "account",
            "mybox-download-day",
            QuotaReset::At { unix_ms: 1_000 },
        )];
        let budget = DurableBudget::open(&path).unwrap();
        configure_costs(&budget, "mybox", Some("plan30gb"), &costs, &[], 1).unwrap();
        budget
            .update_for_tests(|ledger| ledger.reserve(&costs, 2))
            .unwrap();
        drop(budget);
        let reopened = DurableBudget::open(&path).unwrap();
        let summaries = configure_costs(
            &reopened,
            "mybox",
            Some("plan30gb"),
            &costs,
            &[QuotaLimitOverride {
                bucket: "mybox-download-day".into(),
                limit: 400,
            }],
            3,
        )
        .unwrap();
        assert_eq!(summaries[0].limit, Some(400));
        assert_eq!(summaries[0].used, 1);
    }

    #[test]
    fn retry_after_and_rolling_windows_survive_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("quota.sqlite");
        let costs = [cost(
            "account",
            "requests",
            QuotaReset::Rolling { window_ms: 100 },
        )];
        let budget = DurableBudget::open(&path).unwrap();
        configure_costs(
            &budget,
            "onedrive",
            None,
            &costs,
            &[QuotaLimitOverride {
                bucket: "requests".into(),
                limit: 1,
            }],
            10,
        )
        .unwrap();
        budget.record_backoff(&costs, 50).unwrap();
        drop(budget);
        let reopened = DurableBudget::open(&path).unwrap();
        assert_eq!(
            reopened
                .reserve_for_tests(&costs, 49)
                .unwrap_err()
                .retry_at_ms,
            Some(50)
        );
        reopened.reserve_for_tests(&costs, 50).unwrap();
        assert!(reopened.reserve_for_tests(&costs, 109).is_err());
        reopened.reserve_for_tests(&costs, 110).unwrap();
    }
}
