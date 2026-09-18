//! Only MYBOX publishes request allowances that this client tracks durably.
//! These are local consumption estimates, not a server's remaining balance.
use super::{
    contract::{ConnectionConfig, ErrorKind, ProviderError, Result},
    durable_quota::MyboxBudget,
    quota::AccountKey,
};
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MyboxPlan {
    Plan30gb, Plan80gb, Plan180gb, Plan2tb, Plan5tb, Plan10tb, Plan20tb,
}
impl MyboxPlan {
    pub fn parse(profile: Option<&str>) -> Result<Self> {
        Ok(match profile.unwrap_or("plan30gb") {
            "plan30gb" => Self::Plan30gb,
            "plan80gb" => Self::Plan80gb,
            "plan180gb" => Self::Plan180gb,
            "plan2tb" => Self::Plan2tb,
            "plan5tb" => Self::Plan5tb,
            "plan10tb" => Self::Plan10tb,
            "plan20tb" => Self::Plan20tb,
            _ => return Err(ProviderError::new(ErrorKind::Unsupported)),
        })
    }
    fn limits(self) -> (u64, u64, u64) {
        match self {
            Self::Plan30gb => (500, 10, 60),
            Self::Plan80gb => (1_000, 10, 60),
            Self::Plan180gb => (1_000, 30, 240),
            Self::Plan2tb => (2_000, 30, 240),
            Self::Plan5tb => (5_000, 30, 240),
            Self::Plan10tb => (20_000, 30, 240),
            Self::Plan20tb => (50_000, 30, 240),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum MyboxCounter {
    DownloadDay,
    MetadataMinute,
    ListMinute,
    FolderMinute,
    UploadUrlMinute,
    DownloadUrlMinute,
    DeleteMinute,
}
impl MyboxCounter {
    pub const ALL: [Self; 7] = [
        Self::DownloadDay, Self::MetadataMinute, Self::ListMinute, Self::FolderMinute,
        Self::UploadUrlMinute, Self::DownloadUrlMinute, Self::DeleteMinute,
    ];
    pub fn id(self) -> &'static str {
        match self {
            Self::DownloadDay => "mybox-download-day",
            Self::MetadataMinute => "mybox-metadata-minute",
            Self::ListMinute => "mybox-list-minute",
            Self::FolderMinute => "mybox-folder-minute",
            Self::UploadUrlMinute => "mybox-upload-url-minute",
            Self::DownloadUrlMinute => "mybox-download-url-minute",
            Self::DeleteMinute => "mybox-delete-minute",
        }
    }
    pub fn limit(self, plan: MyboxPlan) -> u64 {
        let (downloads, lists, general) = plan.limits();
        match self {
            Self::DownloadDay => downloads,
            Self::ListMinute => lists,
            _ => general,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct MyboxCharge {
    pub plan: MyboxPlan,
    /// A URL issuance and its transfer each reserve a download because the
    /// service does not say which consumes its daily allowance.
    pub counters: Vec<MyboxCounter>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MyboxCounterSummary {
    pub id: String,
    pub limit: u64,
    pub used: u64,
    pub reset_at_ms: Option<u64>,
    pub local_estimate: bool,
}

pub(crate) fn next_daily_reset_ms(now_ms: u64) -> u64 {
    const DAY_MS: u64 = 86_400_000;
    const KST_OFFSET_MS: u64 = 32_400_000;
    let local = now_ms.saturating_add(KST_OFFSET_MS);
    (local - local % DAY_MS).saturating_add(DAY_MS).saturating_sub(KST_OFFSET_MS)
}

/// Does not open a quota database at all for another provider. No guessed
/// capacity, charge weights, reset windows or remaining counters are created.
pub(crate) fn connection_usage(
    budget: &MyboxBudget,
    config: &ConnectionConfig,
    now_ms: u64,
) -> Result<Vec<MyboxCounterSummary>> {
    if config.provider != "mybox" { return Ok(Vec::new()); }
    let api = url::Url::parse(&config.endpoint)
        .map_err(|_| ProviderError::new(ErrorKind::Unsupported))?;
    let account = AccountKey::new("mybox", &api, config.account_id.trim())?;
    budget.snapshot(&account, MyboxPlan::parse(config.profile.as_deref())?, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_plan_keeps_the_actual_download_search_and_api_allowances() {
        for (profile, downloads, search, general) in [
            ("plan30gb", 500, 10, 60), ("plan80gb", 1000, 10, 60),
            ("plan180gb", 1000, 30, 240), ("plan2tb", 2000, 30, 240),
            ("plan5tb", 5000, 30, 240), ("plan10tb", 20000, 30, 240),
            ("plan20tb", 50000, 30, 240),
        ] {
            let plan = MyboxPlan::parse(Some(profile)).unwrap();
            assert_eq!(MyboxCounter::DownloadDay.limit(plan), downloads);
            assert_eq!(MyboxCounter::ListMinute.limit(plan), search);
            for counter in MyboxCounter::ALL {
                if !matches!(counter, MyboxCounter::DownloadDay | MyboxCounter::ListMinute) {
                    assert_eq!(counter.limit(plan), general);
                }
            }
        }
        assert!(MyboxPlan::parse(Some("invented" )).is_err());
    }

    #[test]
    fn daily_reset_is_the_next_korean_midnight() {
        assert_eq!(next_daily_reset_ms(0), 54_000_000);
        assert_eq!(next_daily_reset_ms(53_999_999), 54_000_000);
        assert_eq!(next_daily_reset_ms(54_000_000), 140_400_000);
    }

    #[test]
    fn another_provider_has_no_virtual_quota_or_quota_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mybox.sqlite");
        let budget = MyboxBudget::new(path.clone());
        let config = ConnectionConfig {
            provider: "s3".into(), profile: Some("r2".into()),
            endpoint: "https://synthetic.invalid".into(), account_id: "account".into(),
            location: Default::default(), oauth_profile: None,
        };
        assert!(connection_usage(&budget, &config, 1).unwrap().is_empty());
        assert!(!path.exists());
    }
}
