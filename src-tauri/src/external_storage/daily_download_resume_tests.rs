use super::{
    capabilities::Capabilities,
    capture::CaptureCatalog,
    contract::*,
    durable_quota::MyboxBudget,
    fake::{self, FakeProvider, FixedClock},
    http::Clock,
    journal::{JobIdentity, TransferJournal},
    packaging::{self, CompletedSnapshot, PackageLimits, SnapshotMetadata},
    quota::AccountKey,
    quota_profiles::{MyboxCharge, MyboxCounter, MyboxPlan},
    snapshot_restore,
};
use crate::persistent_store::{
    content_capture::ContentCaptureSink, external_capture::CapturedSnapshot,
    sync_selection::CaptureIdentity,
};
use risunest_external_storage_format::{
    content_identity::{hash, hash_reader},
    pack::ENTRY_OVERHEAD,
    snapshot as wire,
};
use std::{
    fs::File,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

const ACCOUNT: &str = "synthetic-account";
const FIRST_RESET_MS: u64 = 54_000_000;
const PACKS: usize = 520;
const INTERRUPT_AFTER: u64 = 300;

struct QuotaProvider {
    inner: Arc<FakeProvider>,
    budget: MyboxBudget,
    account: AccountKey,
    clock: Arc<FixedClock>,
    pack_reads: AtomicU64,
    interrupt_after: Option<u64>,
    interrupt: Cancellation,
}

impl QuotaProvider {
    fn new(
        inner: Arc<FakeProvider>,
        budget: MyboxBudget,
        clock: Arc<FixedClock>,
        interrupt_after: Option<u64>,
        interrupt: Cancellation,
    ) -> Self {
        Self {
            inner,
            budget,
            account: AccountKey::new(
                "mybox",
                &url::Url::parse("https://synthetic.invalid").unwrap(),
                ACCOUNT,
            )
            .unwrap(),
            clock,
            pack_reads: AtomicU64::new(0),
            interrupt_after,
            interrupt,
        }
    }

    fn pack_reads(&self) -> u64 {
        self.pack_reads.load(Ordering::SeqCst)
    }
}

fn download_charge() -> MyboxCharge {
    MyboxCharge {
        plan: MyboxPlan::Plan30gb,
        counters: vec![MyboxCounter::DownloadDay],
    }
}

impl Provider for QuotaProvider {
    fn open_repository<'a>(
        &'a self,
        config: &'a ConnectionConfig,
        secret: &'a SecretRef,
        mode: OpenMode,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, (RepositoryHandle, Capabilities)> {
        self.inner.open_repository(config, secret, mode, cancel)
    }

    fn read_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        unchanged: Option<&'a VersionToken>,
        sink: &'a mut dyn TransferSink,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ReadReceipt> {
        Box::pin(async move {
            let pack = locator.object.starts_with("pack-");
            if pack {
                self.budget
                    .reserve(&self.account, &download_charge(), self.clock.now_ms())?;
            }
            let receipt = self
                .inner
                .read_object(repository, locator, unchanged, sink, cancel)
                .await?;
            if pack {
                let completed = self.pack_reads.fetch_add(1, Ordering::SeqCst) + 1;
                if self.interrupt_after == Some(completed) {
                    self.interrupt.cancel();
                }
            }
            Ok(receipt)
        })
    }

    fn begin_upload<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Option<ResumeState>> {
        self.inner.begin_upload(repository, intent, cancel)
    }

    fn create_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        source: &'a dyn TransferSource,
        resume: Option<&'a ResumeState>,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectReceipt> {
        self.inner
            .create_object(repository, intent, source, resume, cancel)
    }

    fn compare_exchange_head<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        expected: &'a ExpectedHead,
        head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        self.inner
            .compare_exchange_head(repository, locator, expected, head, cancel)
    }

    fn replace_head<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        self.inner.replace_head(repository, locator, head, cancel)
    }

    fn delete_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ()> {
        self.inner.delete_object(repository, locator, cancel)
    }

    fn list_objects<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        collection: Collection,
        cursor: Option<&'a str>,
        limit: u16,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectPage> {
        self.inner
            .list_objects(repository, collection, cursor, limit, cancel)
    }

    fn reconcile_upload<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        resume: Option<&'a ResumeState>,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, UploadResolution> {
        self.inner
            .reconcile_upload(repository, intent, resume, cancel)
    }

    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> {
        self.inner.head_locator(repository)
    }

}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn pack_plaintext_limit(limits: PackageLimits) -> u64 {
    let available = limits.max_stored_bytes - limits.sdk_overhead_bytes;
    let mut low = 0;
    let mut high = available;
    while low < high {
        let middle = low + (high - low + 1) / 2;
        let header = wire::PublicObjectHeader::new(
            "format-repository".into(),
            format!("pack-{}", "0".repeat(64)),
            wire::ObjectRole::Pack,
            middle,
        )
        .unwrap();
        if wire::envelope_length(&header).unwrap() <= available {
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    low
}

fn incompressible_bytes(length: usize) -> Vec<u8> {
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state as u8
        })
        .collect()
}

fn captured(root: &Path, bytes: &[u8]) -> CapturedSnapshot {
    let directory = root
        .join("external-storage")
        .join("captures")
        .join("capture-daily-resume");
    let objects = root.join("external-storage").join("objects");
    let mut catalog = CaptureCatalog::create(&directory, &objects, None).unwrap();
    let identity = CaptureIdentity {
        store_id: "device".into(),
        library_epoch: "library".into(),
        generation: "generation".into(),
        selection_epoch: "selection".into(),
        revision: 10,
    };
    catalog.begin(&identity, None).unwrap();
    catalog.record("root", bytes).unwrap();
    catalog.finish().unwrap();
    CapturedSnapshot {
        id: "capture-daily-resume".into(),
        identity,
        catalog,
        projected_records: 1,
        shared: false,
    }
}

fn metadata(capture: &CapturedSnapshot) -> SnapshotMetadata {
    SnapshotMetadata {
        snapshot_id: "latest".into(),
        repository_id: "format-repository".into(),
        library_id: capture.identity.library_epoch.clone(),
        author_device_id: capture.identity.store_id.clone(),
        created_at_ms: 10,
        logical_revision: 10,
        purpose: crate::external_storage::packaging::SnapshotPurpose::SyncState {
            epoch: "epoch".into(),
            generation: risunest_sync_wire::head::Sequence::from(1u64),
            parent_sections: std::collections::BTreeMap::new(),
        },
        parent_snapshot_id: None,
        content_fingerprint: capture.catalog.content_fingerprint(&risunest_external_storage_format::format::library_fingerprint_domain()).unwrap(),
    }
}

fn journal(root: &Path, capture: &CapturedSnapshot) -> TransferJournal {
    TransferJournal::open(
        root,
        JobIdentity {
            job_id: "publish-latest".into(),
            connection_id: "connection".into(),
            repository_id: fake::repository().repository_id,
            capture_id: capture.id.clone(),
            capture: capture.identity.clone(),
        },
    )
    .unwrap()
}

fn budget(path: &Path) -> MyboxBudget {
    MyboxBudget::new(path.to_path_buf())
}

fn used(path: &Path, now_ms: u64) -> u64 {
    MyboxBudget::new(path.to_path_buf())
        .snapshot(
            &AccountKey::new(
                "mybox",
                &url::Url::parse("https://synthetic.invalid").unwrap(),
                ACCOUNT,
            )
            .unwrap(),
            MyboxPlan::Plan30gb,
            now_ms,
        )
        .unwrap()
        .into_iter()
        .find(|counter| counter.id == MyboxCounter::DownloadDay.id())
        .unwrap()
        .used
}

fn verified_pack_count(completed: &CompletedSnapshot, staging: &Path) -> usize {
    completed
        .referenced_objects
        .iter()
        .filter(|object| object.role == ObjectRole::Pack)
        .filter(|object| {
            staging
                .join("downloads")
                .join(format!("{}.cipher", object.ciphertext_sha256))
                .is_file()
                && staging
                    .join("plaintext")
                    .join(&object.plaintext_sha256)
                    .is_file()
        })
        .count()
}

#[test]
fn daily_download_resume_preserves_verified_packs_and_durable_budget() {
    runtime().block_on(async {
        let temp = tempfile::tempdir().unwrap();
        let repository_root = temp.path().join("repository");
        let cache_root = temp.path().join("package-cache");
        let transfer_root = temp.path().join("transfer");
        let staging_root = temp.path().join("restore-stage");
        let quota_path = temp.path().join("quota.sqlite");
        let limits = PackageLimits {
            max_stored_bytes: 8 * 1024,
            sdk_overhead_bytes: 0,
            target_plaintext_bytes: 8 * 1024,
        };
        let chunk_bytes = pack_plaintext_limit(limits) - ENTRY_OVERHEAD - 1;
        let source = incompressible_bytes(chunk_bytes as usize * PACKS);
        let source_hash = hex::encode(hash(&source));
        let capture = captured(&repository_root, &source);
        let metadata = metadata(&capture);
        let mut journal = journal(&transfer_root, &capture);
        let provider = Arc::new(FakeProvider::new(true));
        let repository = fake::repository();
        let root_key = [7; 32];
        let completed = packaging::package_and_upload(
            capture,
            Vec::new(),
            &repository_root,
            &cache_root,
            metadata,
            &root_key,
            limits,
            &mut journal,
            provider.as_ref(),
            &repository,
            &Cancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            completed
                .referenced_objects
                .iter()
                .filter(|object| object.role == ObjectRole::Pack)
                .count(),
            PACKS
        );

        let first_cancel = Cancellation::default();
        let first_clock = Arc::new(FixedClock::at(1));
        let first = QuotaProvider::new(
            provider.clone(),
            budget(&quota_path),
            first_clock,
            Some(INTERRUPT_AFTER),
            first_cancel.clone(),
        );
        let error = snapshot_restore::download_snapshot(
            &completed.reference,
            &staging_root,
            &root_key,
            &first,
            &repository,
            &first_cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Cancelled);
        assert_eq!(first.pack_reads(), INTERRUPT_AFTER);
        assert_eq!(verified_pack_count(&completed, &staging_root), 300);
        assert_eq!(
            used(&quota_path, 1),
            300
        );

        let second_cancel = Cancellation::default();
        let second = QuotaProvider::new(
            provider.clone(),
            budget(&quota_path),
            Arc::new(FixedClock::at(2)),
            None,
            second_cancel.clone(),
        );
        let error = snapshot_restore::download_snapshot(
            &completed.reference,
            &staging_root,
            &root_key,
            &second,
            &repository,
            &second_cancel,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind, ErrorKind::DailyQuotaExhausted);
        assert_eq!(second.pack_reads(), 200);
        assert_eq!(verified_pack_count(&completed, &staging_root), 500);
        assert_eq!(
            used(&quota_path, 2),
            500
        );

        let third_cancel = Cancellation::default();
        let third = QuotaProvider::new(
            provider,
            budget(&quota_path),
            Arc::new(FixedClock::at(FIRST_RESET_MS)),
            None,
            third_cancel.clone(),
        );
        let prepared = snapshot_restore::download_snapshot(
            &completed.reference,
            &staging_root,
            &root_key,
            &third,
            &repository,
            &third_cancel,
        )
        .await
        .unwrap();
        assert_eq!(third.pack_reads(), 20);
        assert_eq!(verified_pack_count(&completed, &staging_root), PACKS);
        assert_eq!(prepared.snapshot_id, "latest");
        assert_eq!(prepared.records.len(), 1);
        assert_eq!(prepared.records[0].content_hash, source_hash);
        assert_eq!(prepared.records[0].byte_length, source.len() as u64);
        let mut restored = File::open(&prepared.records[0].path).unwrap();
        assert_eq!(
            hex::encode(hash_reader(&mut restored, source.len() as u64).unwrap()),
            source_hash
        );
        assert_eq!(
            used(&quota_path, FIRST_RESET_MS),
            20
        );
    });
}
