//! Amazon S3 with virtual-hosted addressing by default.
use super::{Addressing, Profile, GIB, MAX_PARTS, MIB};

pub(crate) const PROFILE: Profile = Profile {
    id: "aws",
    fixed_region: None,
    default_addressing: Addressing::Virtual,
    path_addressing_only: false,
    requires_endpoint_path: false,
    conditional_put: true,
    conditional_get: true,
    cas_supported: false,
    checksum_header: true,
    download_redirect: false,
    slow_down_is_rate_limit: true,
    same_key_write_window_ms: None,
    multipart_threshold_bytes: 64 * MIB,
    part_size_bytes: 64 * MIB,
    max_single_put_bytes: 5 * GIB,
    service_max_object_bytes: Some(5 * GIB * MAX_PARTS),
    multipart_lifetime_ms: None,
};
