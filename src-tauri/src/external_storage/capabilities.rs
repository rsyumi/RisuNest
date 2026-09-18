use super::contract::{ErrorKind, ProviderError, PublicationStrategy, Result};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Capabilities {
    pub immutable_create: bool,
    pub direct_complete_read: bool,
    pub atomic_create_head: bool,
    pub conditional_head_update: bool,
    pub stable_head_replace: bool,
    pub head_read_after_write: bool,
    pub head_retry_control: bool,
    pub snapshot_discovery: bool,
    pub lease_operations: bool,
    pub delete_objects: bool,
    pub conditional_get: bool,
    pub range: bool,
    pub resumable_upload: bool,
    pub max_stored_bytes: Option<u64>,
    pub sdk_overhead_bytes: u64,
    pub upload_alignment: u64,
}
impl Capabilities {
    pub fn automatic_strategy(&self) -> Result<PublicationStrategy> {
        if self.require(PublicationStrategy::Cas).is_ok() {
            return Ok(PublicationStrategy::Cas);
        }
        self.require(PublicationStrategy::Sequential)?;
        Ok(PublicationStrategy::Sequential)
    }

    pub fn require(&self, strategy: PublicationStrategy) -> Result<()> {
        let common = self.immutable_create && self.direct_complete_read;
        let supported = match strategy {
            PublicationStrategy::Cas => self.atomic_create_head && self.conditional_head_update,
            PublicationStrategy::Sequential => {
                self.stable_head_replace && self.head_read_after_write && self.head_retry_control
            }
        };
        if common && supported {
            Ok(())
        } else {
            Err(ProviderError::new(ErrorKind::Unsupported))
        }
    }

    pub fn cleanup_supported(&self) -> bool {
        self.snapshot_discovery && self.lease_operations && self.delete_objects
    }

    pub fn require_cleanup(&self) -> Result<()> {
        if self.cleanup_supported() {
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

    fn ready() -> Capabilities {
        Capabilities {
            immutable_create: true,
            direct_complete_read: true,
            atomic_create_head: true,
            conditional_head_update: true,
            stable_head_replace: true,
            head_read_after_write: true,
            head_retry_control: true,
            snapshot_discovery: true,
            lease_operations: true,
            delete_objects: true,
            ..Default::default()
        }
    }

    #[test]
    fn automatic_strategy_prefers_cas_without_changing_existing_requirements() {
        let mut value = ready();
        assert_eq!(value.automatic_strategy().unwrap(), PublicationStrategy::Cas);
        value.conditional_head_update = false;
        assert_eq!(value.automatic_strategy().unwrap(), PublicationStrategy::Sequential);
        assert_eq!(value.require(PublicationStrategy::Cas).unwrap_err().kind, ErrorKind::Unsupported);
        value.head_retry_control = false;
        assert_eq!(value.automatic_strategy().unwrap_err().kind, ErrorKind::Unsupported);
    }

    #[test]
    fn every_publication_strategy_needs_immutable_create_and_complete_reads() {
        for clear in [
            (|value: &mut Capabilities| value.immutable_create = false) as fn(&mut Capabilities),
            |value: &mut Capabilities| value.direct_complete_read = false,
        ] {
            let mut value = ready();
            clear(&mut value);
            assert_eq!(value.automatic_strategy().unwrap_err().kind, ErrorKind::Unsupported);
        }
    }

    #[test]
    fn cleanup_requires_operations_without_gating_publication() {
        assert!(ready().cleanup_supported());
        for clear in [
            (|value: &mut Capabilities| value.snapshot_discovery = false) as fn(&mut Capabilities),
            |value: &mut Capabilities| value.lease_operations = false,
            |value: &mut Capabilities| value.delete_objects = false,
        ] {
            let mut value = ready();
            clear(&mut value);
            assert!(!value.cleanup_supported());
            assert_eq!(value.require_cleanup().unwrap_err().kind, ErrorKind::Unsupported);
            assert!(value.automatic_strategy().is_ok());
        }
    }

    #[test]
    fn actual_size_limits_account_for_both_overheads() {
        let mut value = ready();
        value.max_stored_bytes = Some(100);
        value.sdk_overhead_bytes = 10;
        assert_eq!(value.payload_limit(20).unwrap(), Some(70));
        assert_eq!(value.payload_limit(90).unwrap_err().kind, ErrorKind::FileTooLarge);
        assert_eq!(value.payload_limit(u64::MAX).unwrap_err().kind, ErrorKind::FileTooLarge);
    }

    #[test]
    fn capabilities_contain_operations_and_not_documentation_metadata() {
        let encoded = serde_json::to_value(ready()).unwrap();
        assert_eq!(encoded["immutableCreate"], true);
        assert_eq!(encoded["leaseOperations"], true);
        assert!(encoded.get("evidence").is_none());
        assert!(encoded.get("evidenceUrls").is_none());
        assert!(encoded.get("documentedAt").is_none());
    }
}
