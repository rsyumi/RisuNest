//! Cloudflare R2, including the same-key write interval.
use super::{Addressing, Profile, GIB, MIB, TIB};

pub(crate) const PROFILE: Profile = Profile {
    id: "r2",
    fixed_region: None,
    default_addressing: Addressing::Virtual,
    path_addressing_only: false,
    requires_endpoint_path: false,
    conditional_put: true,
    conditional_get: true,
    cas_supported: true,
    checksum_header: false,
    download_redirect: false,
    slow_down_is_rate_limit: false,
    same_key_write_window_ms: Some(1_000),
    multipart_threshold_bytes: 64 * MIB,
    part_size_bytes: 64 * MIB,
    max_single_put_bytes: 5 * GIB,
    service_max_object_bytes: Some(5 * TIB),
    multipart_lifetime_ms: None,
};
