//! Authenticated repository descriptor transport. Public envelope fields remain
//! untrusted until the secretstream has authenticated the complete object.
use super::{
    contract::{
        Cancellation, Collection, ErrorKind, ObjectIntent, ObjectRole, Provider, ProviderError, ReadReceipt,
        RemoteLocator, RepositoryHandle, Result, UploadResolution,
    },
    transfer::{SpoolSink, SpoolSource},
};
use risunest_external_storage_format::{
    content_identity::hash,
    crypto::derive_key,
    format::Descriptor,
    snapshot::{open_envelope, seal_envelope, ObjectRole as EnvelopeRole, PublicObjectHeader},
};
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
};

const MAX_DESCRIPTOR_PLAINTEXT: u64 = 64 * 1024;
const MAX_DESCRIPTOR_CIPHERTEXT: u64 = 128 * 1024;

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
    let directory = root.join("descriptor-staging");
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

pub(crate) async fn upload(
    root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    cancel: &Cancellation,
) -> Result<RemoteLocator> {
    descriptor.validate().map_err(|_| corrupt())?;
    let plaintext = serde_json::to_vec(descriptor).map_err(|_| corrupt())?;
    if plaintext.len() as u64 > MAX_DESCRIPTOR_PLAINTEXT {
        return Err(corrupt());
    }
    let key = derive_key(root_key, &descriptor.repository_id, "metadata").map_err(|_| corrupt())?;
    let header = PublicObjectHeader::new(
        descriptor.repository_id.clone(),
        descriptor.repository_id.clone(),
        EnvelopeRole::Descriptor,
        plaintext.len() as u64,
    )
    .map_err(|_| corrupt())?;
    let mut encrypted = Vec::new();
    seal_envelope(
        &mut std::io::Cursor::new(&plaintext),
        &mut encrypted,
        &key,
        &header,
    )
    .map_err(|_| corrupt())?;
    if encrypted.len() as u64 > MAX_DESCRIPTOR_CIPHERTEXT {
        return Err(corrupt());
    }

    let temporary = staging_path(root)?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary.0)
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    file.write_all(&encrypted)
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    file.sync_all()
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    drop(file);
    let digest = hex::encode(hash(&encrypted));
    let source = SpoolSource::verified(&temporary.0, encrypted.len() as u64, &digest)?;
    let intent = ObjectIntent {
        repository_id: repository.repository_id.clone(),
        // The descriptor operation identity remains stable across command and
        // process retries, so immutable provider creates can converge.
        job_id: descriptor.repository_id.clone(),
        object_id: descriptor.repository_id.clone(),
        role: ObjectRole::Descriptor,
        byte_length: encrypted.len() as u64,
        sha256: digest,
    };
    let resume = provider.begin_upload(repository, &intent, cancel).await?;
    let receipt = match provider
        .create_object(repository, &intent, &source, resume.as_ref(), cancel)
        .await
    {
        Ok(receipt) => receipt,
        Err(error) if matches!(error.kind, ErrorKind::Transient) => {
            match provider
                .reconcile_upload(repository, &intent, resume.as_ref(), cancel)
                .await?
            {
                UploadResolution::Complete(receipt) => receipt,
                UploadResolution::Conflict => {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed))
                }
                UploadResolution::Resumable(_) | UploadResolution::RestartRequired => {
                    return Err(error)
                }
            }
        }
        Err(error) => return Err(error),
    };
    if !receipt.complete || receipt.byte_length != intent.byte_length {
        return Err(corrupt());
    }
    receipt.locator.validate_for(repository)?;
    Ok(receipt.locator)
}

pub(crate) async fn read(
    root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    locator: &RemoteLocator,
    expected: &Descriptor,
    root_key: &[u8; 32],
    cancel: &Cancellation,
) -> Result<Descriptor> {
    expected.validate().map_err(|_| corrupt())?;
    locator.validate_for(repository)?;
    let temporary = staging_path(root)?;
    let mut sink = SpoolSink::create(&temporary.0, MAX_DESCRIPTOR_CIPHERTEXT)?;
    let receipt = provider
        .read_object(repository, locator, None, &mut sink, cancel)
        .await?;
    let receipt = match receipt {
        ReadReceipt::Body(receipt) if receipt.complete && sink.is_verified() => receipt,
        _ => return Err(corrupt()),
    };
    if receipt.byte_length == 0 || receipt.byte_length > MAX_DESCRIPTOR_CIPHERTEXT {
        return Err(corrupt());
    }
    let ciphertext =
        std::fs::read(&temporary.0).map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    if ciphertext.len() as u64 != receipt.byte_length {
        return Err(corrupt());
    }
    let key = derive_key(root_key, &expected.repository_id, "metadata").map_err(|_| corrupt())?;
    let mut plaintext = Vec::new();
    let header = open_envelope(
        &mut std::io::Cursor::new(ciphertext),
        &mut plaintext,
        &key,
        MAX_DESCRIPTOR_PLAINTEXT,
    )
    .map_err(|_| corrupt())?;
    if header.repository_id != expected.repository_id
        || header.object_id != expected.repository_id
        || header.role != EnvelopeRole::Descriptor
    {
        return Err(corrupt());
    }
    let descriptor = Descriptor::decode(&plaintext).map_err(|_| corrupt())?;
    if &descriptor != expected {
        return Err(corrupt());
    }
    Ok(descriptor)
}

/// Reuses only an authenticated descriptor from an unfinished initialization.
/// Providers must separately reject unlisted payloads and foreign root members.
pub(crate) async fn resume_existing(
    root: &Path,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    expected: &Descriptor,
    root_key: &[u8; 32],
    cancel: &Cancellation,
) -> Result<Option<RemoteLocator>> {
    expected.validate().map_err(|_| corrupt())?;
    let mut descriptors = Vec::new();
    for collection in [
        Collection::Snapshots,
        Collection::BackupPoints,
        Collection::InventoryPages,
        Collection::Leases,
        Collection::Descriptors,
    ] {
        let mut cursor: Option<String> = None;
        let mut seen = BTreeSet::new();
        let mut complete = false;
        for _ in 0..10_000 {
            cancel.check()?;
            let page = provider.list_objects(repository, collection, cursor.as_deref(), 64, cancel).await?;
            if page.objects.len() > 64 {
                return Err(corrupt());
            }
            for object in page.objects {
                object.locator.validate_for(repository)?;
                if !object.complete {
                    return Err(corrupt());
                }
                if collection != Collection::Descriptors || descriptors.len() >= 2 {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                if object.byte_length == 0 || object.byte_length > MAX_DESCRIPTOR_CIPHERTEXT {
                    return Err(corrupt());
                }
                descriptors.push(object.locator);
            }
            match page.next_cursor {
                None => { complete = true; break; }
                Some(next) => {
                    if next.is_empty() || next.len() > 64 * 1024 || !seen.insert(hash(next.as_bytes())) {
                        return Err(corrupt());
                    }
                    cursor = Some(next);
                }
            }
        }
        if !complete {
            return Err(corrupt());
        }
    }
    cancel.check()?;
    let had_descriptors = !descriptors.is_empty();
    let mut descriptor = None;
    for locator in descriptors {
        if read(root, provider, repository, &locator, expected, root_key, cancel)
            .await
            .is_ok()
        {
            if descriptor.replace(locator).is_some() {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
        }
    }
    if descriptor.is_none() && had_descriptors {
        return Err(corrupt());
    }
    cancel.check()?;
    Ok(descriptor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::fake;

    fn expected_descriptor() -> Descriptor {
        Descriptor::new("pending-descriptor-id".into(), None).unwrap()
    }

    #[test]
    fn resume_authenticates_existing_ciphertext_without_uploading_again() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let first_root = tempfile::tempdir().unwrap();
            let provider = fake::FakeProvider::new(true);
            let repository = fake::repository();
            let expected = expected_descriptor();
            let cancel = Cancellation::default();
            assert!(resume_existing(first_root.path(), &provider, &repository, &expected, &[7; 32], &cancel)
                .await.unwrap().is_none());
            provider.set_upload_locator(&expected.repository_id, "opaque-remote-descriptor");
            let locator = upload(first_root.path(), &provider, &repository, &expected, &[7; 32], &cancel)
                .await.unwrap();
            let ciphertext = provider.state.lock().unwrap().objects.get(&locator.object).unwrap().0.clone();
            drop(first_root);
            let restarted_root = tempfile::tempdir().unwrap();
            assert_eq!(resume_existing(restarted_root.path(), &provider, &repository, &expected, &[7; 32], &cancel)
                .await.unwrap(), Some(locator.clone()));
            assert_eq!(provider.upload_attempts(&expected.repository_id), 1);
            assert_eq!(provider.state.lock().unwrap().objects.get(&locator.object).unwrap().0, ciphertext);
        });
    }

    #[test]
    fn resume_rejects_foreign_descriptor_identity_key_and_strategy_without_writes() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for variant in 0..3 {
                let root = tempfile::tempdir().unwrap();
                let provider = fake::FakeProvider::new(true);
                let repository = fake::repository();
                let expected = expected_descriptor();
                let other = match variant {
                    0 => Descriptor::new("foreign-descriptor-id".into(), None).unwrap(),
                    1 => expected.clone(),
                    _ => Descriptor::new(expected.repository_id.clone(), Some(risunest_external_storage_format::format::Strategy::Cas)).unwrap(),
                };
                let key = if variant == 1 { [8; 32] } else { [7; 32] };
                upload(root.path(), &provider, &repository, &other, &key, &Cancellation::default()).await.unwrap();
                let before = provider.uploaded_ids();
                assert!(resume_existing(root.path(), &provider, &repository, &expected, &[7; 32], &Cancellation::default())
                    .await.is_err());
                assert_eq!(provider.uploaded_ids(), before);
                assert!(provider.deletion_order().is_empty());
            }
        });
    }

    #[test]
    fn resume_rejects_published_objects_duplicate_descriptors_and_unsafe_pages() {
        use crate::external_storage::contract::{ObjectPage, ObjectReceipt};
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for role in [ObjectRole::SyncState, ObjectRole::BackupBundle, ObjectRole::BackupPoint, ObjectRole::Lease] {
                let root = tempfile::tempdir().unwrap();
                let provider = fake::FakeProvider::new(true);
                provider.seed("foreign", role, vec![1]);
                assert!(resume_existing(root.path(), &provider, &fake::repository(), &expected_descriptor(), &[7; 32], &Cancellation::default())
                    .await.is_err());
                assert!(provider.uploaded_ids().is_empty());
                assert!(provider.deletion_order().is_empty());
            }
            for variant in 0..5 {
                let root = tempfile::tempdir().unwrap();
                let provider = fake::FakeProvider::new(true);
                let repository = fake::repository();
                let mut receipt = ObjectReceipt {
                    locator: RemoteLocator { connection_identity: repository.connection_identity.clone(), collection: None, object: "descriptor".into() },
                    byte_length: 100, version: None, checksum: None, complete: true,
                };
                match variant {
                    0 => {
                        provider.script_page(Collection::Descriptors, Ok(ObjectPage { objects: vec![receipt.clone()], next_cursor: Some("next".into()) }));
                        provider.script_page(Collection::Descriptors, Ok(ObjectPage { objects: vec![receipt], next_cursor: None }));
                    }
                    1 => {
                        for _ in 0..2 {
                            provider.script_page(Collection::Snapshots, Ok(ObjectPage { objects: vec![], next_cursor: Some("cycle".into()) }));
                        }
                    }
                    2 => {
                        provider.script_page(Collection::Snapshots, Ok(ObjectPage { objects: vec![], next_cursor: Some("next".into()) }));
                        provider.script_page(Collection::Snapshots, Err(ProviderError::new(ErrorKind::Unauthorized)));
                    }
                    3 => {
                        receipt.locator.connection_identity = "foreign".into();
                        provider.script_page(Collection::Descriptors, Ok(ObjectPage { objects: vec![receipt], next_cursor: None }));
                    }
                    _ => {
                        receipt.complete = false;
                        provider.script_page(Collection::Descriptors, Ok(ObjectPage { objects: vec![receipt], next_cursor: None }));
                    }
                }
                assert!(resume_existing(root.path(), &provider, &repository, &expected_descriptor(), &[7; 32], &Cancellation::default())
                    .await.is_err(), "variant {variant}");
                assert!(provider.uploaded_ids().is_empty());
                assert!(provider.deletion_order().is_empty());
            }
        });
    }

    #[test]
    fn encrypted_descriptor_roundtrip_rejects_a_different_recovery_key() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = fake::FakeProvider::new(true);
            let repository = fake::repository();
            let descriptor = Descriptor::new("synthetic-descriptor-id".into(), Some(risunest_external_storage_format::format::Strategy::Cas),
            )
            .unwrap();
            let locator = upload(
                root.path(),
                &provider,
                &repository,
                &descriptor,
                &[7; 32],
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(
                read(
                    root.path(),
                    &provider,
                    &repository,
                    &locator,
                    &descriptor,
                    &[7; 32],
                    &Cancellation::default()
                )
                .await
                .unwrap(),
                descriptor
            );
            assert!(read(
                root.path(),
                &provider,
                &repository,
                &locator,
                &descriptor,
                &[8; 32],
                &Cancellation::default()
            )
            .await
            .is_err());
        });
    }
}
