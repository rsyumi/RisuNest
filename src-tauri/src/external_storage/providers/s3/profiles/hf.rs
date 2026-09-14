//! Hugging Face Storage Buckets through the `s3.hf.co` gateway. The endpoint
//! carries the namespace, only path addressing is served, `If-Match` and
//! `If-None-Match` hold on `PutObject` but not on `GetObject`, downloads answer
//! with a redirect to a CDN edge, and an unfinished multipart upload expires
//! after seven days. Only Storage Buckets are reachable here, never model,
//! dataset or Space repositories.
use super::{Addressing, CostModel, Profile, DOCUMENTED_AT, GIB, MIB};
use crate::external_storage::capabilities::Evidence;

const SEVEN_DAYS_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

pub(crate) const PROFILE: Profile = Profile {
    id: "hf",
    fixed_region: Some("us-east-1"),
    default_addressing: Addressing::Path,
    path_addressing_only: true,
    requires_endpoint_path: true,
    conditional_put: true,
    conditional_get: false,
    // The gateway does not parse trailing checksums and documents no header
    // checksum, so a receipt never claims gateway verification.
    checksum_header: false,
    download_redirect: true,
    slow_down_is_rate_limit: false,
    same_key_write_window_ms: None,
    multipart_threshold_bytes: 64 * MIB,
    part_size_bytes: 64 * MIB,
    max_single_put_bytes: 5 * GIB,
    service_max_object_bytes: None,
    multipart_lifetime_ms: Some(SEVEN_DAYS_MS),
    cas_evidence: Evidence::Synthetic,
    cost_model: CostModel::Flat,
    documented_at: DOCUMENTED_AT,
    evidence_urls: &[
        "https://huggingface.co/docs/hub/storage-buckets-s3",
        "https://huggingface.co/docs/hub/rate-limits",
    ],
};
