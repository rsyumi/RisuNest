use super::contract::{ErrorKind, ProviderError, PublicationStrategy, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum Evidence {
    #[default]
    Unverified,
    Synthetic,
    Live,
}
impl Evidence {
    fn verified(self) -> bool {
        self != Self::Unverified
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Capabilities {
    pub immutable_create: Evidence,
    pub direct_complete_read: Evidence,
    pub atomic_create_head: Evidence,
    pub conditional_head_update: Evidence,
    pub stable_head_replace: Evidence,
    pub head_read_after_write: Evidence,
    pub head_retry_control: Evidence,
    pub snapshot_discovery: Evidence,
    pub discovery_extra_requests: u32,
    pub conditional_get: bool,
    pub range: bool,
    pub resumable_upload: bool,
    pub max_stored_bytes: Option<u64>,
    pub sdk_overhead_bytes: u64,
    pub upload_alignment: u64,
    pub documented_at: Option<String>,
    pub evidence_urls: Vec<String>,
}
impl Capabilities {
    pub fn require(&self, strategy: PublicationStrategy) -> Result<()> {
        let common = self.immutable_create.verified() && self.direct_complete_read.verified();
        let supported = match strategy {
            PublicationStrategy::Cas => {
                self.atomic_create_head.verified() && self.conditional_head_update.verified()
            }
            PublicationStrategy::Sequential => {
                self.stable_head_replace.verified()
                    && self.head_read_after_write.verified()
                    && self.head_retry_control.verified()
            }
        };
        if common && supported {
            Ok(())
        } else {
            Err(ProviderError::new(ErrorKind::Unsupported))
        }
    }
    pub fn payload_limit(&self, format_overhead: u64) -> Result<Option<u64>> {
        self.max_stored_bytes
            .map(|max| {
                max.checked_sub(self.sdk_overhead_bytes)
                    .and_then(|n| n.checked_sub(format_overhead))
                    .filter(|n| *n > 0)
                    .ok_or_else(|| ProviderError::new(ErrorKind::FileTooLarge))
            })
            .transpose()
    }
}
