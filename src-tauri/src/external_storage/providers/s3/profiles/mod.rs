//! Per-service operation declarations and transfer limits for the S3 core.
use crate::external_storage::contract::{ErrorKind, ProviderError, Result};

pub(crate) mod aws;
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

pub(crate) struct Profile {
    pub id: &'static str,
    pub fixed_region: Option<&'static str>,
    pub default_addressing: Addressing,
    pub path_addressing_only: bool,
    pub requires_endpoint_path: bool,
    pub conditional_put: bool,
    pub conditional_get: bool,
    /// Head CAS is a separate declaration from sending conditional object PUTs.
    pub cas_supported: bool,
    pub checksum_header: bool,
    pub download_redirect: bool,
    pub slow_down_is_rate_limit: bool,
    pub same_key_write_window_ms: Option<u64>,
    pub multipart_threshold_bytes: u64,
    pub part_size_bytes: u64,
    pub max_single_put_bytes: u64,
    pub service_max_object_bytes: Option<u64>,
    pub multipart_lifetime_ms: Option<u64>,
}

pub(crate) const MAX_PARTS: u64 = 10_000;
impl Profile {
    pub(crate) fn max_stored_bytes(&self) -> u64 {
        let reachable = self.part_size_bytes.saturating_mul(MAX_PARTS);
        match self.service_max_object_bytes {
            Some(limit) => reachable.min(limit),
            None => reachable,
        }
    }
}

pub(crate) fn lookup(id: &str) -> Result<&'static Profile> {
    match id {
        "aws" => Ok(&aws::PROFILE),
        "r2" => Ok(&r2::PROFILE),
        "b2" => Ok(&b2::PROFILE),
        "hf" => Ok(&hf::PROFILE),
        "generic" => Ok(&generic::PROFILE),
        _ => Err(ProviderError::new(ErrorKind::Unsupported)),
    }
}

pub(crate) const MIB: u64 = 1024 * 1024;
pub(crate) const GIB: u64 = 1024 * MIB;
pub(crate) const TIB: u64 = 1024 * GIB;
