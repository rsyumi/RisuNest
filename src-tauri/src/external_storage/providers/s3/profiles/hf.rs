//! Hugging Face Storage Buckets use a namespace path and explicit CDN redirects.
//! Model, dataset and Space repositories are not reachable through this adapter.
use super::{Addressing, Profile, GIB, MIB};

const SEVEN_DAYS_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

pub(crate) const PROFILE: Profile = Profile {
    id: "hf",
    fixed_region: Some("us-east-1"),
    default_addressing: Addressing::Path,
    path_addressing_only: true,
    requires_endpoint_path: true,
    conditional_put: true,
    conditional_get: false,
    cas_supported: true,
    checksum_header: false,
    download_redirect: true,
    slow_down_is_rate_limit: false,
    same_key_write_window_ms: None,
    multipart_threshold_bytes: 64 * MIB,
    part_size_bytes: 64 * MIB,
    max_single_put_bytes: 5 * GIB,
    service_max_object_bytes: None,
    multipart_lifetime_ms: Some(SEVEN_DAYS_MS),
};
