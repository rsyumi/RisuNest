//! Repository-held recovery bootstrap and optional encrypted connection settings.
use super::{
    connection_store::StoredConnection,
    contract::*,
    transfer::{SpoolSink, SpoolSource},
};
use risunest_external_storage_format::{
    content_identity::hash,
    crypto::{ConnectionSettingsEnvelope, RecoveryKey, RepositoryBootstrapEnvelope},
    format::Descriptor,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
};
use zeroize::Zeroizing;

const BOOTSTRAP_OBJECT_ID: &str = "bootstrap";
const MAX_BOOTSTRAP_OBJECTS: usize = 2;
const MAX_DESCRIPTOR_COLLECTION_OBJECT_BYTES: u64 = 128 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BootstrapMetadata {
    pub descriptor: Descriptor,
    pub descriptor_locator: RemoteLocator,
}

pub(crate) struct ImportedBootstrap {
    pub metadata: BootstrapMetadata,
    pub key: Zeroizing<[u8; 32]>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConnectionSettingsPayload {
    config: ConnectionConfig,
    repository_id: String,
    credential: Option<Vec<u8>>,
    account_id: Option<String>,
}

pub(crate) struct ImportedConnectionSettings {
    pub config: ConnectionConfig,
    pub repository_id: String,
    pub credential: Option<Zeroizing<Vec<u8>>>,
    pub account_id: Option<String>,
}

struct TemporaryFile(PathBuf);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

fn staging_path(root: &Path) -> Result<TemporaryFile> {
    let directory = root.join("recovery-staging");
    std::fs::create_dir_all(&directory).map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    if crate::trust_boundary::is_link_like(
        &std::fs::symlink_metadata(&directory)
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?,
    ) {
        return Err(corrupt());
    }
    Ok(TemporaryFile(
        directory.join(format!("{}.tmp", uuid::Uuid::new_v4())),
    ))
}

pub(crate) fn generate_key() -> Result<Zeroizing<String>> {
    RecoveryKey::generate()
        .map(|value| value.expose())
        .map_err(|_| ProviderError::new(ErrorKind::Transient))
}

fn parse_key(value: &str) -> Result<RecoveryKey> {
    RecoveryKey::parse(value).map_err(|_| corrupt())
}

fn bootstrap_identity(repository: &RepositoryHandle) -> String {
    hex::encode(hash(repository.repository_id.as_bytes()))
}

async fn read_object(
    root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    receipt: &ObjectReceipt,
    cancel: &Cancellation,
) -> Result<Vec<u8>> {
    if !receipt.complete
        || receipt.byte_length == 0
        || receipt.byte_length
            > MAX_DESCRIPTOR_COLLECTION_OBJECT_BYTES
    {
        return Err(corrupt());
    }
    receipt.locator.validate_for(repository)?;
    let temporary = staging_path(root)?;
    let mut sink = SpoolSink::create(&temporary.0, receipt.byte_length)?;
    let read = provider
        .read_object(repository, &receipt.locator, None, &mut sink, cancel)
        .await?;
    match read {
        ReadReceipt::Body(value)
            if value.complete
                && value.byte_length == receipt.byte_length
                && sink.is_verified() => {}
        _ => return Err(corrupt()),
    }
    std::fs::read(&temporary.0).map_err(|_| ProviderError::new(ErrorKind::Transient))
}

async fn scan_bootstrap(
    root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    code: &RecoveryKey,
    cancel: &Cancellation,
) -> Result<(usize, Option<ImportedBootstrap>)> {
    let mut cursor = None;
    let mut visited = BTreeSet::new();
    let mut count = 0usize;
    let mut found = None;
    loop {
        let page = provider
            .list_objects(
                repository,
                Collection::Descriptors,
                cursor.as_deref(),
                8,
                cancel,
            )
            .await?;
        if page.objects.len() > 8 {
            return Err(corrupt());
        }
        for receipt in page.objects {
            count = count.saturating_add(1);
            if count > MAX_BOOTSTRAP_OBJECTS {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
            let bytes = read_object(root, provider, repository, &receipt, cancel).await?;
            if bytes.len()
                > risunest_external_storage_format::crypto::MAX_REPOSITORY_BOOTSTRAP_BYTES
            {
                continue;
            }
            let Ok(envelope) = RepositoryBootstrapEnvelope::decode(&bytes) else {
                continue;
            };
            let identity = bootstrap_identity(repository);
            if found.is_some() || envelope.repository_id != identity {
                return Err(corrupt());
            }
            let recovered = envelope
                .recover(&identity, code)
                .map_err(|_| corrupt())?;
            let metadata: BootstrapMetadata =
                serde_json::from_str(&recovered.connection_metadata).map_err(|_| corrupt())?;
            metadata.descriptor.validate().map_err(|_| corrupt())?;
            metadata.descriptor_locator.validate_for(repository)?;
            found = Some(ImportedBootstrap {
                metadata,
                key: recovered.root,
            });
        }
        match page.next_cursor {
            None => break,
            Some(next)
                if !next.is_empty()
                    && next.len() <= 64 * 1024
                    && visited.len() < 10_000
                    && visited.insert(next.clone()) =>
            {
                cursor = Some(next)
            }
            Some(_) => return Err(corrupt()),
        }
    }
    Ok((count, found))
}

pub(crate) async fn open_bootstrap(
    root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    recovery_key: &str,
    cancel: &Cancellation,
) -> Result<ImportedBootstrap> {
    let code = parse_key(recovery_key)?;
    scan_bootstrap(root, provider, repository, &code, cancel)
        .await?
        .1
        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))
}

pub(crate) async fn publish_bootstrap(
    root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    metadata: &BootstrapMetadata,
    root_key: &[u8; 32],
    recovery_key: &str,
    cancel: &Cancellation,
) -> Result<()> {
    metadata.descriptor.validate().map_err(|_| corrupt())?;
    metadata.descriptor_locator.validate_for(repository)?;
    let code = parse_key(recovery_key)?;
    let (count, existing) = scan_bootstrap(root, provider, repository, &code, cancel).await?;
    if let Some(existing) = existing {
        return if existing.metadata == *metadata && *existing.key == *root_key {
            Ok(())
        } else {
            Err(corrupt())
        };
    }
    if count >= MAX_BOOTSTRAP_OBJECTS {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let envelope = RepositoryBootstrapEnvelope::protect(
        bootstrap_identity(repository),
        serde_json::to_string(metadata).map_err(|_| corrupt())?,
        root_key,
        &code,
    )
    .map_err(|_| corrupt())?;
    let bytes = envelope.encode().map_err(|_| corrupt())?;
    let temporary = staging_path(root)?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary.0)
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    file.write_all(&bytes)
        .and_then(|_| file.sync_all())
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    drop(file);
    let digest = hex::encode(hash(&bytes));
    let source = SpoolSource::verified(&temporary.0, bytes.len() as u64, &digest)?;
    let intent = ObjectIntent {
        repository_id: repository.repository_id.clone(),
        job_id: BOOTSTRAP_OBJECT_ID.into(),
        object_id: BOOTSTRAP_OBJECT_ID.into(),
        role: ObjectRole::Descriptor,
        byte_length: bytes.len() as u64,
        sha256: digest,
    };
    let resume = provider.begin_upload(repository, &intent, cancel).await?;
    let receipt = match provider
        .create_object(repository, &intent, &source, resume.as_ref(), cancel)
        .await
    {
        Ok(receipt) => receipt,
        Err(error) if error.kind == ErrorKind::Transient => match provider
            .reconcile_upload(repository, &intent, resume.as_ref(), cancel)
            .await?
        {
            UploadResolution::Complete(receipt) => receipt,
            UploadResolution::Conflict => {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed))
            }
            UploadResolution::Resumable(_) | UploadResolution::RestartRequired => return Err(error),
        },
        Err(error) => return Err(error),
    };
    if !receipt.complete || receipt.byte_length != intent.byte_length {
        return Err(corrupt());
    }
    let opened = open_bootstrap(root, provider, repository, recovery_key, cancel).await?;
    if opened.metadata != *metadata || *opened.key != *root_key {
        return Err(corrupt());
    }
    Ok(())
}

pub(crate) fn export_connection_settings(
    connection: &StoredConnection,
    recovery_key: &str,
    credential: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let code = parse_key(recovery_key)?;
    let payload = ConnectionSettingsPayload {
        config: connection.config.clone(),
        repository_id: connection.descriptor.repository_id.clone(),
        credential: if connection.config.oauth_profile.is_some() {
            None
        } else {
            credential.map(ToOwned::to_owned)
        },
        account_id: (!connection.config.account_id.is_empty())
            .then(|| connection.config.account_id.clone()),
    };
    let plaintext = Zeroizing::new(serde_json::to_vec(&payload).map_err(|_| corrupt())?);
    ConnectionSettingsEnvelope::protect(
        connection.descriptor.repository_id.clone(),
        &plaintext,
        &code,
    )
    .and_then(|value| value.encode())
    .map_err(|_| corrupt())
}

pub(crate) fn import_connection_settings(
    bytes: &[u8],
    recovery_key: &str,
) -> Result<ImportedConnectionSettings> {
    let code = parse_key(recovery_key)?;
    let envelope = ConnectionSettingsEnvelope::decode(bytes).map_err(|_| corrupt())?;
    let plaintext = envelope
        .open(&envelope.repository_id, &code)
        .map_err(|_| corrupt())?;
    let payload: ConnectionSettingsPayload =
        serde_json::from_slice(&plaintext).map_err(|_| corrupt())?;
    if payload.repository_id != envelope.repository_id
        || payload.repository_id.is_empty()
        || payload.config.provider.is_empty()
        || payload.config.oauth_profile.is_some() && payload.credential.is_some()
    {
        return Err(corrupt());
    }
    Ok(ImportedConnectionSettings {
        config: payload.config,
        repository_id: payload.repository_id,
        credential: payload.credential.map(Zeroizing::new),
        account_id: payload.account_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::{capabilities::Capabilities, descriptor, fake};

    fn stored() -> StoredConnection {
        StoredConnection {
            id: "connection".into(),
            config: ConnectionConfig {
                provider: "webdav".into(),
                profile: None,
                endpoint: "https://synthetic.invalid".into(),
                account_id: "account".into(),
                location: Default::default(),
                oauth_profile: None,
            },
            descriptor: Descriptor::new("repository".into(), None).unwrap(),
            descriptor_locator: fake::locator(),
            provider_repository_id: fake::repository().repository_id,
            credential_ref: "credential".into(),
            root_key_ref: "root".into(),
            recovery_key_ref: "recovery".into(),
            capture_policy: None,
            retention_policy: None,
            capabilities: Capabilities::default(),
            created_at_ms: 1,
            last_sync_at_ms: None,
            last_backup_at_ms: None,
        }
    }

    #[test]
    fn connection_settings_roundtrip_keeps_credentials_encrypted_and_omits_keys() {
        let connection = stored();
        let key = generate_key().unwrap();
        let bytes = export_connection_settings(&connection, &key, Some(b"secret")).unwrap();
        let visible = String::from_utf8_lossy(&bytes);
        assert!(!visible.contains("secret"));
        assert!(!visible.contains(&connection.root_key_ref));
        assert!(!visible.contains(&*key));
        let imported = import_connection_settings(&bytes, &key).unwrap();
        assert!(imported.config == connection.config);
        assert_eq!(imported.repository_id, "repository");
        assert_eq!(imported.credential.unwrap().as_slice(), b"secret");
        assert!(import_connection_settings(&bytes, &generate_key().unwrap()).is_err());
        let mut damaged = bytes;
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        assert!(import_connection_settings(&damaged, &key).is_err());
    }

    #[test]
    fn connection_settings_never_transfer_oauth_tokens() {
        let mut connection = stored();
        connection.config.oauth_profile = Some(OAuthProfile {
            project_id: "project".into(),
            platform_client_ids: Default::default(),
        });
        let key = generate_key().unwrap();
        let bytes = export_connection_settings(&connection, &key, Some(b"oauth-token")).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("oauth-token"));
        let imported = import_connection_settings(&bytes, &key).unwrap();
        assert!(imported.credential.is_none());
    }

    #[test]
    fn repository_bootstrap_roundtrip_refuses_wrong_key_identity_tampering_and_missing_data() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = fake::FakeProvider::new(true);
            let mut repository = fake::repository();
            repository.repository_id = format!("onedrive|{}", "synthetic-provider-identity/".repeat(10));
            let descriptor = Descriptor::new("repository".into(), None).unwrap();
            let root_key = [7; 32];
            let recovery_key = generate_key().unwrap();
            let cancel = Cancellation::default();
            let descriptor_locator = descriptor::upload(
                root.path(),
                &provider,
                &repository,
                &descriptor,
                &root_key,
                &cancel,
            )
            .await
            .unwrap();
            let metadata = BootstrapMetadata { descriptor, descriptor_locator };

            publish_bootstrap(
                root.path(),
                &provider,
                &repository,
                &metadata,
                &root_key,
                &recovery_key,
                &cancel,
            )
            .await
            .unwrap();
            let uploads = provider.uploaded_ids();
            let opened = open_bootstrap(
                root.path(),
                &provider,
                &repository,
                &recovery_key,
                &cancel,
            )
            .await
            .unwrap();
            assert_eq!(opened.metadata, metadata);
            assert_eq!(*opened.key, root_key);

            assert!(open_bootstrap(
                root.path(),
                &provider,
                &repository,
                &generate_key().unwrap(),
                &cancel,
            )
            .await
            .is_err());
            assert_eq!(provider.uploaded_ids(), uploads);

            let mut other_repository = fake::repository();
            other_repository.repository_id = "another-provider-repository".into();
            assert!(open_bootstrap(
                root.path(),
                &provider,
                &other_repository,
                &recovery_key,
                &cancel,
            )
            .await
            .is_err());
            assert_eq!(provider.uploaded_ids(), uploads);

            let code = parse_key(&recovery_key).unwrap();
            let mut tampered = RepositoryBootstrapEnvelope::protect(
                bootstrap_identity(&repository),
                serde_json::to_string(&metadata).unwrap(),
                &root_key,
                &code,
            )
            .unwrap()
            .encode()
            .unwrap();
            let last = tampered.len() - 1;
            tampered[last] ^= 1;
            let damaged = fake::FakeProvider::new(true);
            damaged.seed(BOOTSTRAP_OBJECT_ID, ObjectRole::Descriptor, tampered);
            assert!(open_bootstrap(
                root.path(),
                &damaged,
                &repository,
                &recovery_key,
                &cancel,
            )
            .await
            .is_err());
            assert!(damaged.uploaded_ids().is_empty());

            let missing = fake::FakeProvider::new(true);
            assert!(open_bootstrap(
                root.path(),
                &missing,
                &repository,
                &recovery_key,
                &cancel,
            )
            .await
            .is_err());
            assert!(missing.uploaded_ids().is_empty());
        });
    }
}
