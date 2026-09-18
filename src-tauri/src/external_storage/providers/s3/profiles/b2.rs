//! Backblaze B2 through its S3-compatible API, without conditional head writes.
use super::{Addressing, Profile, GIB, MIB};

pub(crate) const PROFILE: Profile = Profile {
    id: "b2",
    fixed_region: None,
    default_addressing: Addressing::Virtual,
    path_addressing_only: false,
    requires_endpoint_path: false,
    conditional_put: false,
    conditional_get: false,
    cas_supported: false,
    checksum_header: false,
    download_redirect: false,
    slow_down_is_rate_limit: true,
    same_key_write_window_ms: None,
    multipart_threshold_bytes: 64 * MIB,
    part_size_bytes: 64 * MIB,
    max_single_put_bytes: 5 * GIB,
    service_max_object_bytes: None,
    multipart_lifetime_ms: None,
};
