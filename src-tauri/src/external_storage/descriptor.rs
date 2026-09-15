//! Authenticated repository descriptor transport. Public envelope fields remain
//! untrusted until the secretstream has authenticated the complete object.
use super::{
    contract::{
        Cancellation, ErrorKind, ObjectIntent, ObjectRole, Provider, ProviderError, ReadReceipt,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::fake;

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
