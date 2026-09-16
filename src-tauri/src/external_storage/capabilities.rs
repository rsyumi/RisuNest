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
    /// A single object can be removed by name.
    pub delete_objects: Evidence,
    /// A full enumeration of `Leases` and `BackupPoints` started after another
    /// device finished a write contains that object, and paging never skips one.
    pub gc_control_consistency: Evidence,
    /// A success answer means that request will not delete the object later.
    pub delete_completion: Evidence,
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
    /// Every evidence the cleanup path needs. One unverified item disables GC
    /// for this connection; it never disables backup or synchronization.
    pub fn require_cleanup(&self) -> Result<()> {
        if self.delete_objects.verified()
            && self.snapshot_discovery.verified()
            && self.gc_control_consistency.verified()
            && self.delete_completion.verified()
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_needs_all_four_evidences_and_never_gates_publication() {
        let ready = Capabilities {
            immutable_create: Evidence::Synthetic,
            direct_complete_read: Evidence::Synthetic,
            stable_head_replace: Evidence::Synthetic,
            head_read_after_write: Evidence::Synthetic,
            head_retry_control: Evidence::Synthetic,
            snapshot_discovery: Evidence::Synthetic,
            delete_objects: Evidence::Synthetic,
            gc_control_consistency: Evidence::Synthetic,
            delete_completion: Evidence::Synthetic,
            ..Default::default()
        };
        assert!(ready.require_cleanup().is_ok());
        assert!(ready.require(PublicationStrategy::Sequential).is_ok());

        let unverified: [fn(&mut Capabilities); 4] = [
            |value| value.delete_objects = Evidence::Unverified,
            |value| value.snapshot_discovery = Evidence::Unverified,
            |value| value.gc_control_consistency = Evidence::Unverified,
            |value| value.delete_completion = Evidence::Unverified,
        ];
        for clear in unverified {
            let mut value = ready.clone();
            clear(&mut value);
            assert_eq!(
                value.require_cleanup().unwrap_err().kind,
                ErrorKind::Unsupported
            );
            // Backup and synchronization stay available without cleanup evidence.
            assert!(value.require(PublicationStrategy::Sequential).is_ok());
        }
    }
}
