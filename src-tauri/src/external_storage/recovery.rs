//! Independent key recovery carries authenticated connection hints, never local
//! device identity, job cursors, OS aliases or OAuth refresh tokens.
use super::{connection_store::StoredConnection, contract::*};
use risunest_external_storage_format::{
    crypto::{RecoveryCode, RecoveryEnvelope},
    format::Descriptor,
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RecoveryMetadata {
    pub config: ConnectionConfig,
    pub descriptor: Descriptor,
    pub descriptor_locator: RemoteLocator,
    pub provider_repository_id: String,
}
pub(crate) struct ExportedRecovery {
    pub bytes: Vec<u8>,
    pub code: Zeroizing<String>,
}
pub(crate) struct ImportedRecovery {
    pub metadata: RecoveryMetadata,
    pub key: Zeroizing<[u8; 32]>,
}
fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

pub(crate) fn export(connection: &StoredConnection, key: &[u8; 32]) -> Result<ExportedRecovery> {
    let metadata = RecoveryMetadata {
        config: connection.config.clone(),
        descriptor: connection.descriptor.clone(),
        descriptor_locator: connection.descriptor_locator.clone(),
        provider_repository_id: connection.provider_repository_id.clone(),
    };
    let code = RecoveryCode::generate().map_err(|_| corrupt())?;
    let envelope = RecoveryEnvelope::protect(
        metadata.descriptor.repository_id.clone(),
        serde_json::to_string(&metadata).map_err(|_| corrupt())?,
        key,
        &code,
    )
    .map_err(|_| corrupt())?;
    Ok(ExportedRecovery {
        bytes: envelope.encode().map_err(|_| corrupt())?,
        code: code.expose(),
    })
}

/// The caller presents the authenticated endpoint and requires confirmation
/// before invoking an adapter or sending provider credentials.
pub(crate) fn import(bytes: &[u8], code: &str) -> Result<ImportedRecovery> {
    let envelope = RecoveryEnvelope::decode(bytes).map_err(|_| corrupt())?;
    let code = RecoveryCode::parse(code).map_err(|_| corrupt())?;
    let recovered = envelope
        .recover(&envelope.repository_id, &code)
        .map_err(|_| corrupt())?;
    let metadata: RecoveryMetadata =
        serde_json::from_str(&recovered.connection_metadata).map_err(|_| corrupt())?;
    metadata.descriptor.validate().map_err(|_| corrupt())?;
    if metadata.descriptor.repository_id != envelope.repository_id
        || metadata.provider_repository_id.is_empty()
    {
        return Err(corrupt());
    }
    Ok(ImportedRecovery {
        metadata,
        key: recovered.root,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::{capabilities::Capabilities, fake};
    
    #[test]
    fn new_device_recovers_without_source_vault_and_operational_credentials() {
        let descriptor = Descriptor::new("synthetic-repository".into(), None,
        )
        .unwrap();
        let connection = StoredConnection {
            id: "device-connection-id".into(),
            config: ConnectionConfig {
                provider: "s3".into(),
                profile: Some("r2".into()),
                endpoint: "https://synthetic.invalid".into(),
                account_id: "synthetic-account".into(),
                location: Default::default(),
                oauth_profile: None,
            },
            descriptor,
            descriptor_locator: fake::locator(),
            provider_repository_id: fake::repository().repository_id,
            credential_ref: "not-exported-credential".into(),
            root_key_ref: "not-exported-os-key".into(),
            capture_policy: None,
            retention_policy: None,
            capabilities: Capabilities::default(),
            created_at_ms: 1,
            last_sync_at_ms: None,
            last_backup_at_ms: None,
        };
        let exported = export(&connection, &[7; 32]).unwrap();
        // Typical connection fits a static QR without serializing ciphertext
        // as an expansive JSON array of individual integer bytes.
        assert!(exported.bytes.len() < 2900);
        let recovered = import(&exported.bytes, &exported.code).unwrap();
        assert_eq!(*recovered.key, [7; 32]);
        assert_eq!(
            recovered.metadata.config.endpoint,
            "https://synthetic.invalid"
        );
        let metadata = serde_json::to_string(&recovered.metadata).unwrap();
        for secret in [
            "not-exported-credential",
            "not-exported-os-key",
            "device-connection-id",
        ] {
            assert!(!metadata.contains(secret));
        }
        let wrong = RecoveryCode::generate().unwrap();
        assert!(import(&exported.bytes, &wrong.expose()).is_err());
        let mut damaged: serde_json::Value = serde_json::from_slice(&exported.bytes).unwrap();
        damaged["repositoryId"] = "different".into();
        assert!(import(&serde_json::to_vec(&damaged).unwrap(), &exported.code).is_err());
    }
}
