//! One bounded immutable upload attempt. The scheduler owns waiting/backoff;
//! uncertain results are reconciled before any subsequent payload request.
use super::{
    contract::*,
    journal::{validate_receipt, TransferJournal},
    transfer::SpoolSource,
};

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
    if let Some(receipt) = record.receipt {
        validate_receipt(&record.intent, repository, &receipt)?;
        return Ok(receipt);
    }
    // Reopen and verify before a request can consume the source after a restart.
    let path = journal.spool_path(object);
    let length = record.intent.byte_length;
    let digest = record.intent.sha256.clone();
    let source = tokio::task::spawn_blocking(move || SpoolSource::verified(&path, length, &digest))
        .await
        .map_err(|_| ProviderError::new(ErrorKind::Transient))??;
    if record.attempted {
        match provider
            .reconcile_upload(repository, &record.intent, record.resume.as_ref(), cancel)
            .await?
        {
            UploadResolution::Complete(receipt) => {
                journal.complete(&record.intent, repository, &receipt)?;
                return Ok(receipt);
            }
            UploadResolution::Resumable(resume) => record.resume = Some(resume),
            UploadResolution::RestartRequired => record.resume = None,
            UploadResolution::Conflict => {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed))
            }
        }
    }
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
                        journal.complete(&record.intent, repository, &receipt)?;
                        return Ok(receipt);
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
