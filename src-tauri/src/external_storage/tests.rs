use super::{capabilities::*, contract::*, publication::*, quota::*, registry::Registry};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

fn capabilities(cas: bool) -> Capabilities {
    Capabilities {
        immutable_create: Evidence::Synthetic,
        direct_complete_read: Evidence::Synthetic,
        atomic_create_head: if cas {
            Evidence::Synthetic
        } else {
            Evidence::Unverified
        },
        conditional_head_update: if cas {
            Evidence::Synthetic
        } else {
            Evidence::Unverified
        },
        stable_head_replace: Evidence::Synthetic,
        head_read_after_write: Evidence::Synthetic,
        head_retry_control: Evidence::Synthetic,
        ..Default::default()
    }
}

#[derive(Default)]
struct FakeState {
    objects: BTreeMap<String, (Vec<u8>, u64)>,
    next_version: u64,
    lose_response: bool,
}
struct FakeProvider {
    state: Mutex<FakeState>,
    cas: bool,
}
impl FakeProvider {
    fn new(cas: bool) -> Self {
        Self {
            state: Mutex::new(FakeState::default()),
            cas,
        }
    }
    fn write(
        &self,
        locator: &RemoteLocator,
        expected: Option<&ExpectedHead>,
        bytes: &[u8],
    ) -> Result<HeadReceipt> {
        if expected.is_some() && !self.cas {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        let mut state = self.state.lock().unwrap();
        let previous = state.objects.get(&locator.object);
        if let Some(expected) = expected {
            let matches = match (expected, previous) {
                (ExpectedHead::Absent, None) => true,
                (ExpectedHead::Exact(token), Some((_, v))) => token.0 == v.to_string(),
                _ => false,
            };
            if !matches {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
        }
        state.next_version += 1;
        let version = state.next_version;
        state
            .objects
            .insert(locator.object.clone(), (bytes.into(), version));
        if std::mem::take(&mut state.lose_response) {
            return Err(ProviderError::new(ErrorKind::Transient));
        }
        Ok(HeadReceipt {
            version: Some(VersionToken(version.to_string())),
            complete: true,
        })
    }
}
impl Provider for FakeProvider {
    fn open_repository<'a>(
        &'a self,
        _: &'a ConnectionConfig,
        _: &'a SecretRef,
        _: OpenMode,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, (RepositoryHandle, Capabilities)> {
        Box::pin(async move {
            c.check()?;
            Ok((repository(), capabilities(self.cas)))
        })
    }
    fn read_object<'a>(
        &'a self,
        r: &'a RepositoryHandle,
        l: &'a RemoteLocator,
        unchanged: Option<&'a VersionToken>,
        sink: &'a mut dyn TransferSink,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, ReadReceipt> {
        Box::pin(async move {
            use tokio::io::AsyncWriteExt;
            c.check()?;
            l.validate_for(r)?;
            let (bytes, version) = self
                .state
                .lock()
                .unwrap()
                .objects
                .get(&l.object)
                .cloned()
                .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
            let token = VersionToken(version.to_string());
            if unchanged == Some(&token) {
                return Ok(ReadReceipt::NotModified(token));
            }
            let hash = risunest_sync_wire::hash(&bytes);
            let mut writer = sink.open(0, bytes.len() as u64, c).await?;
            writer
                .write_all(&bytes)
                .await
                .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
            writer
                .shutdown()
                .await
                .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
            drop(writer);
            sink.finish(bytes.len() as u64, &hash).await?;
            Ok(ReadReceipt::Body(ObjectReceipt {
                locator: l.clone(),
                byte_length: bytes.len() as u64,
                version: Some(token),
                checksum: None,
                complete: true,
            }))
        })
    }
    fn create_object<'a>(
        &'a self,
        r: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        source: &'a dyn TransferSource,
        _: Option<&'a ResumeState>,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectReceipt> {
        Box::pin(async move {
            use tokio::io::AsyncReadExt;
            c.check()?;
            intent.validate(r)?;
            if intent.byte_length > 1024 * 1024 || source.byte_length() != intent.byte_length {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            let mut bytes = Vec::new();
            source
                .open(0, intent.byte_length, c)
                .await?
                .take(intent.byte_length + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
            if bytes.len() as u64 != intent.byte_length
                || risunest_sync_wire::hash(&bytes) != intent.sha256
            {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let locator = RemoteLocator {
                connection_identity: r.connection_identity.clone(),
                collection: None,
                object: intent.object_id.clone(),
            };
            let receipt = {
                let mut state = self.state.lock().unwrap();
                if let Some((old, _)) = state.objects.get(&intent.object_id) {
                    if old != &bytes {
                        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                    }
                } else {
                    state.next_version += 1;
                    let v = state.next_version;
                    state.objects.insert(intent.object_id.clone(), (bytes, v));
                }
                let (_, v) = state.objects.get(&intent.object_id).unwrap();
                ObjectReceipt {
                    locator,
                    byte_length: intent.byte_length,
                    version: Some(VersionToken(v.to_string())),
                    checksum: None,
                    complete: true,
                }
            };
            Ok(receipt)
        })
    }
    fn compare_exchange_head<'a>(
        &'a self,
        r: &'a RepositoryHandle,
        l: &'a RemoteLocator,
        e: &'a ExpectedHead,
        h: &'a HeadBytes,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move {
            c.check()?;
            l.validate_for(r)?;
            self.write(l, Some(e), h.as_bytes())
        })
    }
    fn replace_head<'a>(
        &'a self,
        r: &'a RepositoryHandle,
        l: &'a RemoteLocator,
        h: &'a HeadBytes,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move {
            c.check()?;
            l.validate_for(r)?;
            self.write(l, None, h.as_bytes())
        })
    }
    fn list_objects<'a>(
        &'a self,
        _: &'a RepositoryHandle,
        _: Collection,
        _: Option<&'a str>,
        _: u16,
        _: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectPage> {
        Box::pin(async { Err(ProviderError::new(ErrorKind::Unsupported)) })
    }
    fn reconcile_upload<'a>(
        &'a self,
        _: &'a RepositoryHandle,
        _: &'a ObjectIntent,
        _: &'a ResumeState,
        _: &'a Cancellation,
    ) -> ProviderFuture<'a, UploadResolution> {
        Box::pin(async { Ok(UploadResolution::RestartRequired) })
    }
    fn request_cost(&self, _: ProviderOperation) -> Vec<RequestCost> {
        Vec::new()
    }
}
fn repository() -> RepositoryHandle {
    RepositoryHandle {
        repository_id: "synthetic-repository".into(),
        connection_identity: "synthetic-account/root".into(),
        context: Box::new(()),
    }
}
fn locator() -> RemoteLocator {
    RemoteLocator {
        connection_identity: repository().connection_identity,
        collection: None,
        object: "head".into(),
    }
}
fn observation(commit: &str) -> HeadObservation {
    HeadObservation {
        commit_id: commit.into(),
        authenticated_body_hash: risunest_sync_wire::hash(commit.as_bytes()),
        version: None,
    }
}

#[test]
fn cas_two_writers_have_exactly_one_winner_for_create_and_update() {
    for initial in [false, true] {
        let provider = Arc::new(FakeProvider::new(true));
        let expected = if initial {
            provider
                .write(&locator(), Some(&ExpectedHead::Absent), b"first")
                .unwrap();
            ExpectedHead::Exact(VersionToken("1".into()))
        } else {
            ExpectedHead::Absent
        };
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = (0..2)
            .map(|n| {
                let p = provider.clone();
                let b = barrier.clone();
                let e = expected.clone();
                std::thread::spawn(move || {
                    b.wait();
                    futures::executor::block_on(p.compare_exchange_head(
                        &repository(),
                        &locator(),
                        &e,
                        &HeadBytes::new(vec![n]).unwrap(),
                        &Cancellation::default(),
                    ))
                    .is_ok()
                })
            })
            .collect();
        barrier.wait();
        assert_eq!(
            workers
                .into_iter()
                .map(|t| usize::from(t.join().unwrap()))
                .sum::<usize>(),
            1
        );
    }
}

#[test]
fn sequential_competitors_can_both_confirm_and_recovery_snapshots_survive() {
    let p = FakeProvider::new(false);
    p.write(
        &RemoteLocator {
            object: "snapshot-a".into(),
            ..locator()
        },
        None,
        b"a",
    )
    .unwrap();
    p.write(
        &RemoteLocator {
            object: "snapshot-b".into(),
            ..locator()
        },
        None,
        b"b",
    )
    .unwrap();
    let mut a = Attempt::new(
        &capabilities(false),
        PublicationStrategy::Sequential,
        None,
        "a".into(),
        observation("a").authenticated_body_hash,
    )
    .unwrap();
    let mut b = Attempt::new(
        &capabilities(false),
        PublicationStrategy::Sequential,
        None,
        "b".into(),
        observation("b").authenticated_body_hash,
    )
    .unwrap();
    a.before_write(None, ExecutionSession::Foreground).unwrap();
    b.before_write(None, ExecutionSession::Foreground).unwrap();
    p.write(&locator(), None, b"a").unwrap();
    assert_eq!(
        a.observe_result(Some(&observation("a"))),
        Outcome::Confirmed
    );
    p.write(&locator(), None, b"b").unwrap();
    assert_eq!(
        b.observe_result(Some(&observation("b"))),
        Outcome::Confirmed
    );
    assert_eq!(p.state.lock().unwrap().objects.len(), 3);
    assert_eq!(
        p.write(&locator(), Some(&ExpectedHead::Absent), b"x")
            .unwrap_err()
            .kind,
        ErrorKind::Unsupported
    );
}

#[test]
fn response_loss_is_reconciled_by_observation_and_never_repeated_as_old_write() {
    let p = FakeProvider::new(false);
    let mut a = Attempt::new(
        &capabilities(false),
        PublicationStrategy::Sequential,
        None,
        "a".into(),
        observation("a").authenticated_body_hash,
    )
    .unwrap();
    a.before_write(None, ExecutionSession::ExitDrain).unwrap();
    p.state.lock().unwrap().lose_response = true;
    let error = p.write(&locator(), None, b"a").unwrap_err();
    assert_eq!(a.write_failed(&error), Outcome::PublicationUnknown);
    assert!(a.before_write(None, ExecutionSession::Foreground).is_err());
    assert_eq!(
        a.observe_result(Some(&observation("b"))),
        Outcome::PublicationUnknown
    );
    assert_eq!(
        a.observe_result(Some(&observation("a"))),
        Outcome::Confirmed
    );
    assert_eq!(p.state.lock().unwrap().next_version, 1);
}

#[test]
fn sequential_hidden_and_changed_or_missing_head_cannot_publish() {
    let expected = observation("base");
    let mut a = Attempt::new(
        &capabilities(false),
        PublicationStrategy::Sequential,
        Some(expected.clone()),
        "a".into(),
        observation("a").authenticated_body_hash,
    )
    .unwrap();
    assert_eq!(
        a.before_write(Some(&expected), ExecutionSession::Hidden)
            .unwrap_err()
            .kind,
        ErrorKind::Cancelled
    );
    assert!(a.before_write(None, ExecutionSession::Foreground).is_err());
    assert!(a
        .before_write(Some(&expected), ExecutionSession::Foreground)
        .is_err());
    assert!(Attempt::new(
        &capabilities(false),
        PublicationStrategy::Cas,
        None,
        "a".into(),
        observation("a").authenticated_body_hash
    )
    .is_err());
}

#[test]
fn quota_is_shared_persistent_atomic_and_does_not_reset_on_restore_or_retry() {
    let mut ledger = QuotaLedger::default();
    ledger.configure(
        "account",
        "download",
        Bucket {
            limit: 500,
            used: 499,
            reset: QuotaReset::At { unix_ms: 1000 },
            blocked_until_ms: None,
            last_reset_ms: None,
        },
    );
    let cost = RequestCost {
        bucket: "download".into(),
        shared_account: "account".into(),
        units: 1,
        reset: QuotaReset::At { unix_ms: 1000 },
    };
    assert!(ledger.reserve(&[cost.clone(), cost.clone()], 500).is_err());
    assert_eq!(ledger.used("account", "download"), Some(499));
    ledger.reserve(&[cost.clone()], 500).unwrap();
    let mut reopened: QuotaLedger =
        serde_json::from_str(&serde_json::to_string(&ledger).unwrap()).unwrap();
    assert_eq!(
        reopened.reserve(&[cost.clone()], 999).unwrap_err().kind,
        ErrorKind::DailyQuotaExhausted
    );
    reopened.reserve(&[cost.clone()], 1000).unwrap();
    reopened.configure(
        "account",
        "download",
        Bucket {
            limit: 500,
            used: 0,
            reset: QuotaReset::At { unix_ms: 1000 },
            blocked_until_ms: None,
            last_reset_ms: None,
        },
    );
    reopened.reserve(&[cost], 1001).unwrap();
    assert_eq!(reopened.used("account", "download"), Some(2));
}

#[test]
fn unavailable_services_stay_hidden_and_head_size_and_sdk_overhead_are_bounded() {
    let registry = Registry::default();
    assert!(registry.available().is_empty());
    assert!(registry.get("s3").is_err());
    assert!(HeadBytes::new(vec![0; HeadBytes::MAX_BYTES + 1]).is_err());
    let c = Capabilities {
        max_stored_bytes: Some(250_000_000),
        sdk_overhead_bytes: 100,
        ..Default::default()
    };
    assert_eq!(c.payload_limit(64).unwrap(), Some(249_999_836));
    assert!(c.payload_limit(250_000_000).is_err());
}

#[test]
fn bounded_spool_round_trip_is_idempotent_and_rejects_corruption_and_overrun() {
    use super::transfer::{SpoolSink, SpoolSource};
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        use tokio::io::AsyncWriteExt;
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source");
        let bytes = vec![42; 100_005];
        std::fs::write(&source_path, &bytes).unwrap();
        let digest = risunest_sync_wire::hash(&bytes);
        let source = SpoolSource::verified(&source_path, bytes.len() as u64, &digest).unwrap();
        let p = FakeProvider::new(false);
        let repository = repository();
        let cancel = Cancellation::default();
        let intent = ObjectIntent {
            repository_id: repository.repository_id.clone(),
            job_id: "job".into(),
            object_id: "pack".into(),
            role: ObjectRole::Pack,
            byte_length: bytes.len() as u64,
            sha256: digest.clone(),
        };
        let first = p
            .create_object(&repository, &intent, &source, None, &cancel)
            .await
            .unwrap();
        assert_eq!(
            first,
            p.create_object(&repository, &intent, &source, None, &cancel)
                .await
                .unwrap()
        );
        let output = dir.path().join("received");
        let mut sink = SpoolSink::create(&output, bytes.len() as u64).unwrap();
        p.read_object(&repository, &first.locator, None, &mut sink, &cancel)
            .await
            .unwrap();
        assert!(sink.is_verified());
        assert_eq!(std::fs::read(&output).unwrap(), bytes);
        assert!(source.open(bytes.len() as u64, 1, &cancel).await.is_err());
        let mut limited = SpoolSink::create(&dir.path().join("limited"), 4).unwrap();
        let mut writer = limited.open(0, 4, &cancel).await.unwrap();
        assert!(writer.write_all(b"12345").await.is_err());
        drop(writer);
        assert!(!limited.is_verified());
        assert!(limited.finish(4, &digest).await.is_err());
        cancel.cancel();
        assert!(source.open(0, 1, &cancel).await.is_err());
    });
}
