//! Amazon S3 itself. It shares the documented request shapes with the generic
//! preset, and differs in the two places Amazon publishes for its own service:
//! virtual-hosted addressing is the default, and the consistency model states
//! that a delete and a listing started after it both hold.
use super::{Addressing, CostModel, Profile, DOCUMENTED_AT, GIB, MAX_PARTS, MIB};
use crate::external_storage::capabilities::Evidence;

pub(crate) const PROFILE: Profile = Profile {
    id: "aws",
    fixed_region: None,
    default_addressing: Addressing::Virtual,
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
    // Conditional writes are left unverified until that documentation is read.
    cas_evidence: Evidence::Unverified,
    // `DeleteObject` answers 204 and is permanent while versioning is off, and
    // the consistency model covers a listing started after a write finished.
    // A multi-page walk is not covered: nothing states that a concurrent change
    // cannot make `ListObjectsV2` skip a key across page boundaries.
    cleanup_evidence: Evidence::Synthetic,
    cost_model: CostModel::Flat,
    documented_at: DOCUMENTED_AT,
    evidence_urls: &[
        "https://docs.aws.amazon.com/AmazonS3/latest/API/API_PutObject.html",
        "https://docs.aws.amazon.com/AmazonS3/latest/API/API_DeleteObject.html",
        "https://docs.aws.amazon.com/AmazonS3/latest/userguide/Welcome.html#ConsistencyModel",
    ],
};
