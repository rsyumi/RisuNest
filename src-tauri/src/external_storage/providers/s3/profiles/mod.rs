//! Per-service presets for the one S3 core. A preset only records what the
//! service's own documentation states; anything undocumented stays off and is
//! reported as unverified rather than inferred from another S3 service.
use crate::external_storage::{
    capabilities::Evidence,
    contract::{ErrorKind, ProviderError, ProviderOperation, QuotaReset, RequestCost, Result},
};

pub(crate) mod b2;
pub(crate) mod generic;
pub(crate) mod hf;
pub(crate) mod r2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Addressing {
    Path,
    Virtual,
}
impl Addressing {
    pub(crate) fn parse(value: &str) -> Result<Self> {
        match value {
            "path" => Ok(Self::Path),
            "virtual" => Ok(Self::Virtual),
            _ => Err(ProviderError::new(ErrorKind::Unsupported)),
        }
    }
}

/// How one request maps onto the account buckets the service documents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CostModel {
    /// Cloudflare bills R2 as Class A (mutations, listings) and Class B (reads).
    ClassAb,
    /// Backblaze documents a per-second ceiling on upload/download requests on
    /// top of the account transaction count.
    Transactions,
    /// No published per-request accounting; one unit per request, no known reset.
    Flat,
}

pub(crate) struct Profile {
    pub id: &'static str,
    /// Region the service fixes for every bucket, when it documents one.
    pub fixed_region: Option<&'static str>,
    pub default_addressing: Addressing,
    /// The service documents that virtual-hosted addressing is not served.
    pub path_addressing_only: bool,
    /// The endpoint carries a namespace path segment the service requires.
    pub requires_endpoint_path: bool,
    /// `If-None-Match` / `If-Match` are documented on `PutObject`.
    pub conditional_put: bool,
    /// Conditional `GetObject` is documented; drives `Capabilities::conditional_get`.
    pub conditional_get: bool,
    /// The service documents that a `PutObject` response confirms a SHA-256
    /// checksum header, so a receipt may claim provider verification.
    pub checksum_header: bool,
    /// `GetObject` answers with a redirect that has to be followed explicitly.
    pub download_redirect: bool,
    /// The service documents 503 as its throttling status rather than an outage.
    pub slow_down_is_rate_limit: bool,
    /// Documented minimum interval between writes to one key, if any.
    pub same_key_write_window_ms: Option<u64>,
    /// Objects at or below this size go up in one `PutObject`.
    pub multipart_threshold_bytes: u64,
    pub part_size_bytes: u64,
    pub max_single_put_bytes: u64,
    /// Documented maximum object size, when the service publishes one.
    pub service_max_object_bytes: Option<u64>,
    /// Documented lifetime of an unfinished multipart upload.
    pub multipart_lifetime_ms: Option<u64>,
    /// Evidence class for the two conditional head capabilities.
    pub cas_evidence: Evidence,
    pub cost_model: CostModel,
    pub documented_at: &'static str,
    pub evidence_urls: &'static [&'static str],
}

/// S3 caps a multipart upload at 10,000 parts, which bounds what this adapter
/// can store regardless of the service maximum.
pub(crate) const MAX_PARTS: u64 = 10_000;

impl Profile {
    /// Largest object this adapter can actually put in place on the service.
    pub(crate) fn max_stored_bytes(&self) -> u64 {
        let reachable = self.part_size_bytes.saturating_mul(MAX_PARTS);
        match self.service_max_object_bytes {
            Some(limit) => reachable.min(limit),
            None => reachable,
        }
    }
    pub(crate) fn costs(&self, account: &str, operation: ProviderOperation) -> Vec<RequestCost> {
        // The adapter signs every request itself and issues no download URL, so
        // neither operation ever reaches the service.
        if matches!(
            operation,
            ProviderOperation::DownloadUrl | ProviderOperation::Authenticate
        ) {
            return Vec::new();
        }
        let unit = |bucket: &str, reset: QuotaReset| RequestCost {
            bucket: bucket.into(),
            shared_account: account.into(),
            units: 1,
            reset,
        };
        let reads = matches!(
            operation,
            ProviderOperation::Metadata | ProviderOperation::Get | ProviderOperation::Range
        );
        let mut costs = match self.cost_model {
            CostModel::ClassAb if reads => vec![unit("class_b", QuotaReset::Unknown)],
            CostModel::ClassAb => vec![unit("class_a", QuotaReset::Unknown)],
            CostModel::Transactions => {
                let mut costs = vec![unit("transactions", QuotaReset::Unknown)];
                if !matches!(operation, ProviderOperation::List) {
                    costs.push(unit(
                        "requests_per_second",
                        QuotaReset::Rolling { window_ms: 1_000 },
                    ));
                }
                costs
            }
            CostModel::Flat => vec![unit("requests", QuotaReset::Unknown)],
        };
        if let Some(window_ms) = self.same_key_write_window_ms {
            if matches!(
                operation,
                ProviderOperation::CompareExchangeHead | ProviderOperation::ReplaceHead
            ) {
                costs.push(unit("head_key_write", QuotaReset::Rolling { window_ms }));
            }
        }
        costs
    }
}

pub(crate) fn lookup(id: &str) -> Result<&'static Profile> {
    match id {
        "r2" => Ok(&r2::PROFILE),
        "b2" => Ok(&b2::PROFILE),
        "hf" => Ok(&hf::PROFILE),
        "generic" => Ok(&generic::PROFILE),
        _ => Err(ProviderError::new(ErrorKind::Unsupported)),
    }
}

/// Documentation every preset relies on for the shared S3 request shapes.
pub(crate) const SHARED_EVIDENCE: &[&str] = &[
    "https://docs.aws.amazon.com/IAM/latest/UserGuide/create-signed-request.html",
    "https://docs.aws.amazon.com/AmazonS3/latest/userguide/conditional-writes.html",
    "https://docs.aws.amazon.com/AmazonS3/latest/userguide/qfacts.html",
];

pub(crate) const MIB: u64 = 1024 * 1024;
pub(crate) const GIB: u64 = 1024 * MIB;
pub(crate) const TIB: u64 = 1024 * GIB;
/// Documentation read date shared by every preset in this adapter.
pub(crate) const DOCUMENTED_AT: &str = "2026-09-14";
