//! Any other S3-compatible endpoint. It claims nothing beyond what Amazon
//! documents for S3 itself, and the two conditional head capabilities stay
//! unverified because an arbitrary implementation behind the endpoint has not
//! been shown to honour them.
use super::{Addressing, CostModel, Profile, DOCUMENTED_AT, GIB, MAX_PARTS, MIB};
use crate::external_storage::capabilities::Evidence;

pub(crate) const PROFILE: Profile = Profile {
    id: "generic",
    fixed_region: None,
    // Path addressing is what a custom host or gateway serves reliably.
    default_addressing: Addressing::Path,
    path_addressing_only: false,
    requires_endpoint_path: false,
    conditional_put: true,
    conditional_get: true,
    checksum_header: true,
    download_redirect: false,
    slow_down_is_rate_limit: true,
    same_key_write_window_ms: None,
    multipart_threshold_bytes: 64 * MIB,
    part_size_bytes: 64 * MIB,
    max_single_put_bytes: 5 * GIB,
    // 10,000 parts of at most 5 GiB, as the multipart limits table states.
    service_max_object_bytes: Some(5 * GIB * MAX_PARTS),
    multipart_lifetime_ms: None,
    cas_evidence: Evidence::Unverified,
    cost_model: CostModel::Flat,
    documented_at: DOCUMENTED_AT,
    evidence_urls: &["https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutObject.html"],
};
