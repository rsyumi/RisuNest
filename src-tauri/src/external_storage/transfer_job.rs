//! One bounded immutable upload attempt. The scheduler owns waiting/backoff;
//! uncertain results are reconciled before any subsequent payload request.
use super::{
    contract::*,
    journal::{validate_receipt, TransferJournal},
    transfer::{SpoolSink, SpoolSource},
};
use risunest_external_storage_format::{content_identity::hash, format::Descriptor};

/// Reconcile answers identify an object, but not every provider supplies a
/// verified SHA-256. In that case authenticate the bytes through a bounded
/// download instead of accepting an old receipt or a same-sized object.
pub(crate) async fn verify_remote_receipt(
    directory: &std::path::Path,
    intent: &ObjectIntent,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    receipt: ObjectReceipt,
    cancel: &Cancellation,
) -> Result<Option<ObjectReceipt>> {
    verify_remote_receipt_at(directory, intent, provider, repository, receipt, cancel, None, None).await
}

pub(super) async fn verify_remote_receipt_at(
    directory: &std::path::Path,
    intent: &ObjectIntent,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    receipt: ObjectReceipt,
    cancel: &Cancellation,
    unchanged: Option<&VersionToken>,
    output: Option<&std::path::Path>,
) -> Result<Option<ObjectReceipt>> {
    validate_receipt(intent, repository, &receipt)?;
    if output.is_none() && unchanged.is_none() && receipt.checksum.as_ref().is_some_and(|checksum| {
        checksum.provider_verified
            && checksum.algorithm.eq_ignore_ascii_case("sha256")
            && checksum.value == intent.sha256
    }) {
        return Ok(Some(receipt));
    }
    let temporary = tempfile::tempdir_in(directory)
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let path = output.map(std::path::Path::to_path_buf).unwrap_or_else(|| temporary.path().join("object"));
    let mut sink = SpoolSink::create(&path, intent.byte_length)?;
    let current = match provider.read_object(repository, &receipt.locator, unchanged, &mut sink, cancel).await {
        Ok(ReadReceipt::Body(current)) => current,
        Ok(ReadReceipt::NotModified(version)) => {
            if unchanged == Some(&version) { return Ok(Some(receipt)); }
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        Err(error) if error.kind == ErrorKind::NotFound => {
            std::fs::remove_file(&path).map_err(|_| ProviderError::new(ErrorKind::Transient))?;
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    validate_receipt(intent, repository, &current)?;
    if current.locator != receipt.locator || !sink.is_verified() {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let length = intent.byte_length;
    let digest = intent.sha256.clone();
    tokio::task::spawn_blocking(move || SpoolSource::verified(&path, length, &digest))
        .await
        .map_err(|_| ProviderError::new(ErrorKind::Transient))??;
    Ok(Some(current))
}

pub(crate) async fn upload(
    journal: &mut TransferJournal,
    object: &str,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<ObjectReceipt> {
    upload_inner(journal, object, provider, repository, cancel, true).await
}

pub(crate) async fn upload_inventory(
    journal: &mut TransferJournal,
    object: &str,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<ObjectReceipt> {
    upload_inner(journal, object, provider, repository, cancel, false).await
}

async fn upload_inner(
    journal: &mut TransferJournal,
    object: &str,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
    recreate_missing: bool,
) -> Result<ObjectReceipt> {
    cancel.check()?;
    let mut record = journal
        .record(object)?
        .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
    record.intent.validate(repository)?;
    if let Some(receipt) = &record.receipt {
        validate_receipt(&record.intent, repository, receipt)?;
    }
    if record.attempted || record.receipt.is_some() {
        // Completed sessions may already have released their sealed secret.
        // An unfinished session is still useful and must not be discarded.
        let resume = if record.receipt.is_some() { None } else { record.resume.as_ref() };
        match provider.reconcile_upload(repository, &record.intent, resume, cancel).await? {
            UploadResolution::Complete(receipt) => {
                if let Some(receipt) = verify_remote_receipt(
                    journal.directory(), &record.intent, provider, repository, receipt, cancel,
                ).await? {
                    journal.complete(&record.intent, repository, &receipt)?;
                    return Ok(receipt);
                }
                if !recreate_missing {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                record.resume = None;
            }
            UploadResolution::Resumable(resume) => record.resume = Some(resume),
            UploadResolution::RestartRequired => {
                // An attempted registration may have been retired even when its
                // creation response was lost before a local receipt was saved.
                if !recreate_missing {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                record.resume = None;
            }
            UploadResolution::Conflict => {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
        }
        journal.reopen_object(object)?;
    }
    // Only missing objects need the original ciphertext, including its nonce.
    let path = journal.spool_path(object);
    let length = record.intent.byte_length;
    let digest = record.intent.sha256.clone();
    let source = tokio::task::spawn_blocking(move || {
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(ProviderError::new(ErrorKind::NotFound));
            }
            Err(_) => return Err(ProviderError::new(ErrorKind::Transient)),
            Ok(_) => {}
        }
        SpoolSource::verified(&path, length, &digest)
    })
        .await
        .map_err(|_| ProviderError::new(ErrorKind::Transient))??;
    if record.resume.is_none() {
        // A crash in begin_upload may leave an empty remote session, but cannot
        // lose a payload upload whose session was never recorded locally.
        record.resume = provider
            .begin_upload(repository, &record.intent, cancel)
            .await?;
    }
    journal.attempted(object, record.resume.as_ref())?;
    let outcome = provider
        .create_object(
            repository,
            &record.intent,
            &source,
            record.resume.as_ref(),
            cancel,
        )
        .await;
    match outcome {
        Ok(receipt) => {
            journal.complete(&record.intent, repository, &receipt)?;
            Ok(receipt)
        }
        Err(original) => {
            // Cancellation and exhausted budgets defer observation to the next
            // allowed session. Every failure retains its attempted state.
            if cancel.check().is_ok()
                && !matches!(
                    original.kind,
                    ErrorKind::DailyQuotaExhausted | ErrorKind::RateLimited | ErrorKind::Cancelled
                )
            {
                match provider
                    .reconcile_upload(repository, &record.intent, record.resume.as_ref(), cancel)
                    .await
                {
                    Ok(UploadResolution::Complete(receipt)) => {
                        if let Some(receipt) = verify_remote_receipt(
                            journal.directory(), &record.intent, provider, repository, receipt, cancel,
                        ).await? {
                            journal.complete(&record.intent, repository, &receipt)?;
                            return Ok(receipt);
                        }
                    }
                    Ok(UploadResolution::Resumable(resume)) => {
                        journal.attempted(object, Some(&resume))?
                    }
                    Ok(UploadResolution::RestartRequired) => journal.attempted(object, None)?,
                    Ok(UploadResolution::Conflict) => {
                        return Err(ProviderError::new(ErrorKind::PreconditionFailed))
                    }
                    Err(_) => (),
                }
            }
            Err(original)
        }
    }
}

pub(crate) async fn upload_registered(
    journal: &mut TransferJournal,
    object: &str,
    format_repository_id: &str,
    root_key: &[u8; 32],
    plaintext_length: u64,
    plaintext_sha256: &str,
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    cancel: &Cancellation,
) -> Result<ObjectReceipt> {
    let descriptor = Descriptor::new(format_repository_id.to_owned(), None)
        .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    let record = journal
        .record(object)?
        .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
    if !matches!(
        record.intent.role,
        ObjectRole::Pack
            | ObjectRole::Catalog
            | ObjectRole::SyncState
            | ObjectRole::BackupBundle
            | ObjectRole::BackupPoint
    ) {
        return upload(journal, object, provider, repository, cancel).await;
    }
    let page_id = inventory_page_id(journal.job_id(), object, &record.intent.sha256);
    let document = super::control::inventory_page_document(
        &descriptor,
        repository,
        journal.job_id(),
        &page_id,
        &[super::control::InventoryRegistration {
            intent: &record.intent,
            plaintext_length,
            plaintext_sha256,
        }],
    )?;
    Box::pin(super::control::upload_inventory_page(
        &descriptor,
        root_key,
        document,
        journal,
        provider,
        repository,
        cancel,
    ))
    .await?;
    upload(journal, object, provider, repository, cancel).await
}

fn inventory_page_id(job_id: &str, object: &str, sha256: &str) -> String {
    let mut identity = Vec::with_capacity(job_id.len() + object.len() + 66);
    identity.extend_from_slice(job_id.as_bytes());
    identity.push(0);
    identity.extend_from_slice(object.as_bytes());
    identity.push(0);
    identity.extend_from_slice(sha256.as_bytes());
    hex::encode(hash(&identity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::{
        fake::{self, FakeProvider},
        journal::JobIdentity,
    };
    use crate::persistent_store::sync_selection::CaptureIdentity;
    use std::io::Write;

    fn identity() -> JobIdentity {
        JobIdentity {
            job_id: "synthetic-job".into(),
            connection_id: "synthetic-connection".into(),
            repository_id: fake::repository().repository_id,
            capture_id: "synthetic-capture".into(),
            capture: CaptureIdentity {
                store_id: "store".into(),
                library_epoch: "epoch".into(),
                generation: "generation".into(),
                selection_epoch: "selection".into(),
                revision: 1,
            },
        }
    }
    fn prepare(root: &std::path::Path) -> (TransferJournal, ObjectIntent) {
        let mut journal = TransferJournal::open(root, identity()).unwrap();
        let bytes = b"synthetic immutable ciphertext";
        let intent = ObjectIntent {
            repository_id: identity().repository_id,
            job_id: identity().job_id,
            object_id: "pack-1".into(),
            role: ObjectRole::Pack,
            byte_length: bytes.len() as u64,
            sha256: risunest_sync_wire::hash(bytes),
        };
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(journal.spool_path(&intent.object_id))
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        journal.register(&intent).unwrap();
        (journal, intent)
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    #[test]
    fn c_reopened_complete_receipt_repairs_only_the_missing_object() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (mut journal, first) = prepare(root.path());
            let mut second = first.clone();
            second.object_id = "pack-2".into();
            std::fs::copy(journal.spool_path(&first.object_id), journal.spool_path(&second.object_id)).unwrap();
            journal.register(&second).unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let cancel = Cancellation::default();
            for intent in [&first, &second] {
                upload(&mut journal, &intent.object_id, &provider, &repository, &cancel).await.unwrap();
                assert!(journal.spool_path(&intent.object_id).exists());
            }
            provider.forget(&first.object_id);
            drop(journal);
            let mut journal = TransferJournal::open(root.path(), identity()).unwrap();
            for intent in [&first, &second] {
                upload(&mut journal, &intent.object_id, &provider, &repository, &cancel).await.unwrap();
            }
            assert!(provider.holds(&first.object_id));
            assert_eq!(provider.upload_attempts(&first.object_id), 2);
            assert_eq!(provider.upload_attempts(&second.object_id), 1);
            assert_eq!(provider.reconcile_attempts(&first.object_id), 1);
            assert_eq!(provider.reconcile_attempts(&second.object_id), 1);
            assert!(provider.read_attempts(&second.object_id) > 0);
        });
    }

    #[test]
    fn c_missing_remote_and_missing_spool_is_not_reported_complete() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (mut journal, intent) = prepare(root.path());
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let cancel = Cancellation::default();
            upload(&mut journal, &intent.object_id, &provider, &repository, &cancel).await.unwrap();
            provider.forget(&intent.object_id);
            std::fs::remove_file(journal.spool_path(&intent.object_id)).unwrap();
            let error = upload(&mut journal, &intent.object_id, &provider, &repository, &cancel).await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::NotFound);
            assert!(journal.record(&intent.object_id).unwrap().unwrap().receipt.is_none());
            assert_eq!(provider.upload_attempts(&intent.object_id), 1);
        });
    }

    #[test]
    fn c_a_present_complete_object_can_be_verified_without_an_old_spool() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (mut journal, intent) = prepare(root.path());
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let cancel = Cancellation::default();
            let original = upload(&mut journal, &intent.object_id, &provider, &repository, &cancel).await.unwrap();
            std::fs::remove_file(journal.spool_path(&intent.object_id)).unwrap();
            assert_eq!(upload(&mut journal, &intent.object_id, &provider, &repository, &cancel).await.unwrap(), original);
            assert_eq!(provider.upload_attempts(&intent.object_id), 1);
            assert_eq!(provider.read_attempts(&intent.object_id), 1);
        });
    }

    #[test]
    fn c_failed_verification_is_not_mistaken_for_a_missing_object() {
        runtime().block_on(async {
            for kind in [ErrorKind::Unauthorized, ErrorKind::Transient, ErrorKind::RateLimited] {
                let root = tempfile::tempdir().unwrap();
                let (mut journal, intent) = prepare(root.path());
                let provider = FakeProvider::new(false);
                let repository = fake::repository();
                let cancel = Cancellation::default();
                upload(&mut journal, &intent.object_id, &provider, &repository, &cancel).await.unwrap();
                provider.fail_read(&intent.object_id, kind);
                let error = upload(&mut journal, &intent.object_id, &provider, &repository, &cancel).await.unwrap_err();
                assert_eq!(error.kind, kind);
                assert_eq!(provider.upload_attempts(&intent.object_id), 1);
                assert!(journal.spool_path(&intent.object_id).exists());
                assert!(journal.record(&intent.object_id).unwrap().unwrap().receipt.is_some());
            }
        });
    }

    #[test]
    fn c_cancelled_completed_upload_does_not_send_a_new_request_or_discard_spool() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (mut journal, intent) = prepare(root.path());
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let cancel = Cancellation::default();
            upload(&mut journal, &intent.object_id, &provider, &repository, &cancel).await.unwrap();
            cancel.cancel();
            assert_eq!(upload(&mut journal, &intent.object_id, &provider, &repository, &cancel).await.unwrap_err().kind, ErrorKind::Cancelled);
            assert_eq!(provider.reconcile_attempts(&intent.object_id), 0);
            assert!(journal.spool_path(&intent.object_id).exists());
        });
    }

    #[test]
    fn lost_single_request_response_is_reconciled_without_session_or_duplicate() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (mut journal, intent) = prepare(root.path());
            let provider = FakeProvider::new(false);
            provider.state.lock().unwrap().lose_response = true;
            let receipt = upload(
                &mut journal,
                &intent.object_id,
                &provider,
                &fake::repository(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert!(receipt.complete);
            assert_eq!(provider.state.lock().unwrap().objects.len(), 1);
            drop(journal);
            let mut journal = TransferJournal::open(root.path(), identity()).unwrap();
            assert!(journal
                .record(&intent.object_id)
                .unwrap()
                .unwrap()
                .receipt
                .is_some());
            assert_eq!(
                upload(
                    &mut journal,
                    &intent.object_id,
                    &provider,
                    &fake::repository(),
                    &Cancellation::default()
                )
                .await
                .unwrap(),
                receipt
            );
            assert_eq!(provider.state.lock().unwrap().next_version, 1);
        });
    }
    #[test]
    fn restart_reconciles_a_previously_sent_object_before_opening_another_upload() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (mut journal, intent) = prepare(root.path());
            let provider = FakeProvider::new(false);
            journal.attempted(&intent.object_id, None).unwrap();
            let source = SpoolSource::verified(
                &journal.spool_path(&intent.object_id),
                intent.byte_length,
                &intent.sha256,
            )
            .unwrap();
            provider
                .create_object(
                    &fake::repository(),
                    &intent,
                    &source,
                    None,
                    &Cancellation::default(),
                )
                .await
                .unwrap();
            drop(journal);
            let mut journal = TransferJournal::open(root.path(), identity()).unwrap();
            upload(
                &mut journal,
                &intent.object_id,
                &provider,
                &fake::repository(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(provider.state.lock().unwrap().next_version, 1);
        });
    }
    #[test]
    fn missing_or_changed_spool_and_different_library_identity_do_not_upload() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (mut journal, intent) = prepare(root.path());
            let provider = FakeProvider::new(false);
            std::fs::write(journal.spool_path(&intent.object_id), b"changed").unwrap();
            assert!(upload(
                &mut journal,
                &intent.object_id,
                &provider,
                &fake::repository(),
                &Cancellation::default()
            )
            .await
            .is_err());
            std::fs::remove_file(journal.spool_path(&intent.object_id)).unwrap();
            assert!(upload(
                &mut journal,
                &intent.object_id,
                &provider,
                &fake::repository(),
                &Cancellation::default()
            )
            .await
            .is_err());
            assert!(provider.state.lock().unwrap().objects.is_empty());
            drop(journal);
            let mut wrong = identity();
            wrong.capture.library_epoch = "restored-copy".into();
            assert!(TransferJournal::open(root.path(), wrong).is_err());
        });
    }
    #[test]
    fn incomplete_foreign_or_wrong_length_receipts_are_not_completion() {
        let root = tempfile::tempdir().unwrap();
        let (mut journal, intent) = prepare(root.path());
        journal.attempted(&intent.object_id, None).unwrap();
        let mut receipt = ObjectReceipt {
            locator: RemoteLocator {
                connection_identity: fake::repository().connection_identity,
                collection: None,
                object: intent.object_id.clone(),
            },
            byte_length: intent.byte_length,
            version: None,
            checksum: None,
            complete: false,
        };
        assert!(journal
            .complete(&intent, &fake::repository(), &receipt)
            .is_err());
        receipt.complete = true;
        receipt.byte_length += 1;
        assert!(journal
            .complete(&intent, &fake::repository(), &receipt)
            .is_err());
        receipt.byte_length -= 1;
        receipt.locator.connection_identity = "another-root".into();
        assert!(journal
            .complete(&intent, &fake::repository(), &receipt)
            .is_err());
        assert!(journal
            .record(&intent.object_id)
            .unwrap()
            .unwrap()
            .receipt
            .is_none());
    }

    #[test]
    fn inventory_registration_failure_sends_no_payload() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (mut journal, intent) = prepare(root.path());
            let provider = FakeProvider::new(false);
            provider.fail_inventory_upload(ErrorKind::Unauthorized);
            let error = upload_registered(
                &mut journal,
                &intent.object_id,
                "format-repository",
                &[7; 32],
                intent.byte_length,
                &intent.sha256,
                &provider,
                &fake::repository(),
                &Cancellation::default(),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind, ErrorKind::Unauthorized);
            assert_eq!(provider.upload_attempts(&intent.object_id), 0);
            assert!(!provider.holds(&intent.object_id));
        });
    }

    #[test]
    fn lost_inventory_response_is_resolved_before_payload_upload() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (mut journal, intent) = prepare(root.path());
            let provider = FakeProvider::new(false);
            provider.state.lock().unwrap().lose_response = true;
            upload_registered(
                &mut journal,
                &intent.object_id,
                "format-repository",
                &[7; 32],
                intent.byte_length,
                &intent.sha256,
                &provider,
                &fake::repository(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let page = format!(
                "inventory-page-{}",
                inventory_page_id(&identity().job_id, &intent.object_id, &intent.sha256)
            );
            assert!(provider.holds(&page));
            assert!(provider.holds(&intent.object_id));
            assert_eq!(provider.upload_attempts(&page), 1);
            assert_eq!(provider.upload_attempts(&intent.object_id), 1);
        });
    }

    #[test]
    fn retired_inventory_coverage_makes_an_old_operation_non_resumable() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (mut journal, intent) = prepare(root.path());
            let provider = FakeProvider::new(false);
            upload_registered(
                &mut journal,
                &intent.object_id,
                "format-repository",
                &[7; 32],
                intent.byte_length,
                &intent.sha256,
                &provider,
                &fake::repository(),
                &Cancellation::default(),
            )
            .await
            .unwrap();
            let page = format!(
                "inventory-page-{}",
                inventory_page_id(&identity().job_id, &intent.object_id, &intent.sha256)
            );
            provider.forget(&page);
            drop(journal);
            let mut journal = TransferJournal::open(root.path(), identity()).unwrap();
            let error = upload_registered(
                &mut journal,
                &intent.object_id,
                "format-repository",
                &[7; 32],
                intent.byte_length,
                &intent.sha256,
                &provider,
                &fake::repository(),
                &Cancellation::default(),
            )
            .await
            .unwrap_err();
            assert_eq!(error.kind, ErrorKind::PreconditionFailed);
            assert_eq!(provider.upload_attempts(&intent.object_id), 1);
        });
    }

    #[test]
    fn missing_inventory_with_a_lost_confirmation_cannot_be_recreated() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let (mut journal, intent) = prepare(root.path());
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let page_id = inventory_page_id(journal.job_id(), &intent.object_id, &intent.sha256);
            let document = super::super::control::inventory_page_document(
                &Descriptor::new("format-repository".into(), None).unwrap(),
                &repository, journal.job_id(), &page_id,
                &[super::super::control::InventoryRegistration {
                    intent: &intent, plaintext_length: intent.byte_length,
                    plaintext_sha256: &intent.sha256,
                }],
            ).unwrap();
            super::super::control::upload_inventory_page(
                &Descriptor::new("format-repository".into(), None).unwrap(),
                &[7; 32], document, &mut journal, &provider, &repository,
                &Cancellation::default(),
            ).await.unwrap();
            let page_object = format!("inventory-page-{page_id}");
            // Model a crash after remote creation but before recording its response.
            journal.reopen_object(&page_object).unwrap();
            assert!(journal.record(&page_object).unwrap().unwrap().attempted);
            provider.forget(&page_object);
            drop(journal);
            let mut journal = TransferJournal::open(root.path(), identity()).unwrap();
            let error = upload_registered(
                &mut journal, &intent.object_id, "format-repository", &[7; 32],
                intent.byte_length, &intent.sha256, &provider, &repository,
                &Cancellation::default(),
            ).await.unwrap_err();
            assert_eq!(error.kind, ErrorKind::PreconditionFailed);
            assert_eq!(provider.upload_attempts(&page_object), 1);
            assert_eq!(provider.upload_attempts(&intent.object_id), 0);
        });
    }

    #[test]
    fn inventory_growth_across_upload_cycles_and_retries_is_measured() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let provider = FakeProvider::new(false);
            let repository = fake::repository();
            let cancel = Cancellation::default();
            let mut payload_requests = 0;
            let mut inventory_requests = 0;
            for cycle in 0..3 {
                let mut job = identity();
                job.job_id = format!("cycle-{cycle}");
                let directory = root.path().join(&job.job_id);
                let mut journal = TransferJournal::open(&directory, job.clone()).unwrap();
                for index in 0..32 {
                    let bytes = format!("synthetic-{cycle}-{index}").into_bytes();
                    let intent = ObjectIntent {
                        repository_id: repository.repository_id.clone(),
                        job_id: job.job_id.clone(),
                        object_id: format!("pack-{cycle}-{index}"),
                        role: ObjectRole::Pack,
                        byte_length: bytes.len() as u64,
                        sha256: risunest_sync_wire::hash(&bytes),
                    };
                    std::fs::write(journal.spool_path(&intent.object_id), &bytes).unwrap();
                    journal.register(&intent).unwrap();
                    for _ in 0..2 {
                        upload_registered(
                            &mut journal, &intent.object_id, "format-repository", &[7; 32],
                            intent.byte_length, &intent.sha256, &provider, &repository, &cancel,
                        ).await.unwrap();
                    }
                    payload_requests += provider.upload_attempts(&intent.object_id);
                    inventory_requests += provider.upload_attempts(&format!("inventory-page-{}",
                        inventory_page_id(&job.job_id, &intent.object_id, &intent.sha256)));
                }
            }
            let state = provider.state.lock().unwrap();
            let pages = state.objects.iter().filter(|(id, _)| id.starts_with("inventory-page-"))
                .map(|(_, (bytes, _))| bytes.len()).collect::<Vec<_>>();
            assert_eq!(pages.len(), 96);
            assert_eq!(payload_requests, 96);
            assert_eq!(inventory_requests, 96);
            let metadata_bytes: usize = pages.iter().sum();
            assert!(metadata_bytes < 96 * 2048);
            eprintln!("3 cycles, 96 payloads, 96 retries: {} inventory pages, {metadata_bytes} encrypted metadata bytes, {payload_requests} payload creates, {inventory_requests} inventory creates", pages.len());
        });
    }

    #[test]
    fn one_object_registration_cost_is_explicit_for_a_representative_cycle() {
        let representative_objects = 10_000usize;
        let root = tempfile::tempdir().unwrap();
        let (_, intent) = prepare(root.path());
        let descriptor = Descriptor::new("format-repository".into(), None).unwrap();
        let document = super::super::control::inventory_page_document(
            &descriptor,
            &fake::repository(),
            &identity().job_id,
            "representative-page",
            &[super::super::control::InventoryRegistration {
                intent: &intent,
                plaintext_length: intent.byte_length,
                plaintext_sha256: &intent.sha256,
            }],
        )
        .unwrap();
        let page_bytes = document
            .encode(risunest_external_storage_format::control::MAX_CONTROL_BYTES)
            .unwrap()
            .len();

        assert_eq!(representative_objects, 10_000);
        assert_eq!(representative_objects * 2, 20_000);
        assert!(page_bytes * representative_objects < 16 * 1024 * 1024);
    }
}
