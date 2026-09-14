//! Backblaze B2 through its S3-compatible API. The published `PutObject`
//! header list carries no `If-Match` or `If-None-Match`, so conditional head
//! writes stay unavailable here instead of being inherited from another S3
//! service. Throttling is documented as 503 SlowDown with `Retry-After`.
use super::{Addressing, CostModel, Profile, DOCUMENTED_AT, GIB, MIB};
use crate::external_storage::capabilities::Evidence;

pub(crate) const PROFILE: Profile = Profile {
    id: "b2",
    fixed_region: None,
    default_addressing: Addressing::Virtual,
    path_addressing_only: false,
    requires_endpoint_path: false,
    conditional_put: false,
    conditional_get: false,
    checksum_header: false,
    download_redirect: false,
    slow_down_is_rate_limit: true,
    same_key_write_window_ms: None,
    multipart_threshold_bytes: 64 * MIB,
    part_size_bytes: 64 * MIB,
    max_single_put_bytes: 5 * GIB,
    service_max_object_bytes: None,
    multipart_lifetime_ms: None,
    cas_evidence: Evidence::Unverified,
    cost_model: CostModel::Transactions,
    documented_at: DOCUMENTED_AT,
    evidence_urls: &[
        "https://www.backblaze.com/apidocs/s3-put-object",
        "https://www.backblaze.com/apidocs/s3-create-multipart-upload",
        "https://www.backblaze.com/docs/cloud-storage-rate-limits",
    ],
};
