//! Cloudflare R2. The S3 compatibility table lists `If-Match`/`If-None-Match`
//! on `PutObject` and on `GetObject`, and the platform limits page documents one
//! write per second to the same key, answered with 429 above that rate.
use super::{Addressing, CostModel, Profile, DOCUMENTED_AT, GIB, MIB, TIB};
use crate::external_storage::capabilities::Evidence;

pub(crate) const PROFILE: Profile = Profile {
    id: "r2",
    fixed_region: None,
    default_addressing: Addressing::Virtual,
    path_addressing_only: false,
    requires_endpoint_path: false,
    conditional_put: true,
    conditional_get: true,
    // The compatibility table lists no x-amz-checksum header on PutObject.
    checksum_header: false,
    download_redirect: false,
    slow_down_is_rate_limit: false,
    same_key_write_window_ms: Some(1_000),
    multipart_threshold_bytes: 64 * MIB,
    part_size_bytes: 64 * MIB,
    max_single_put_bytes: 5 * GIB,
    service_max_object_bytes: Some(5 * TIB),
    multipart_lifetime_ms: None,
    cas_evidence: Evidence::Synthetic,
    cost_model: CostModel::ClassAb,
    documented_at: DOCUMENTED_AT,
    evidence_urls: &[
        "https://developers.cloudflare.com/r2/api/s3/api/",
        "https://developers.cloudflare.com/r2/platform/limits/",
    ],
};
