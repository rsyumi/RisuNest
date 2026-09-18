//! One bounded immutable upload attempt. The scheduler owns waiting/backoff;
//! uncertain results are reconciled before any subsequent payload request.
use super::{
    contract::*,
    journal::{validate_receipt, TransferJournal},
    transfer::{SpoolSink, SpoolSource},
};

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
    validate_receipt(intent, repository, &receipt)?;
    if receipt.checksum.as_ref().is_some_and(|checksum| {
        checksum.provider_verified
            && checksum.algorithm.eq_ignore_ascii_case("sha256")
            && checksum.value == intent.sha256
    }) {
        return Ok(Some(receipt));
    }
    let temporary = tempfile::tempdir_in(directory)
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let path = temporary.path().join("object");
    let mut sink = SpoolSink::create(&path, intent.byte_length)?;
    let current = match provider.read_object(repository, &receipt.locator, None, &mut sink, cancel).await {
        Ok(ReadReceipt::Body(current)) => current,
        Ok(ReadReceipt::NotModified(_)) => return Err(ProviderError::new(ErrorKind::Corrupt)),
        Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
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
                record.resume = None;
            }
            UploadResolution::Resumable(resume) => record.resume = Some(resume),
            UploadResolution::RestartRequired => record.resume = None,
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
}
