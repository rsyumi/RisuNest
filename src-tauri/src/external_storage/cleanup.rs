//! Removing what nothing points at any more. The decision is made by the mark;
//! what happens here is the part that cannot be taken back, so every step
//! between the decision and the first request exists to make another device's
//! work visible. Age is never evidence, and a request whose end is unknown
//! keeps the marker that makes everyone else wait.
use super::{
    capabilities::Capabilities,
    connection::{
        decide_retention, RetentionBundle, RetentionPoint, RetentionPolicy,
    },
    connection_commands::ConnectedRepository,
    contract::{
        Cancellation, Collection, ErrorKind, ObjectReceipt, ProviderError, ProviderFuture, Result,
    },
    control,
    gc_store::GcStore,
    journal::TransferJournal,
    leases::{self, LeaseContext, LeaseHandle},
    packaging::RemoteObject,
    reachability::{self, DocumentNode, DocumentSource, MarkRequest, Roots},
};
use risunest_external_storage_format::control::BundleSource;
use std::collections::{BTreeMap, BTreeSet};

/// How many removals one run sends, and how many it sends between two readings
/// of the lease collection.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CleanupLimits {
    pub batch: usize,
    pub per_run: usize,
}
impl Default for CleanupLimits {
    fn default() -> Self {
        Self {
            batch: 25,
            per_run: 200,
        }
    }
}

/// Why a run stopped. Only `complete` means the committed list is empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    Limit,
    Budget,
    Lease,
    Complete,
    Uncertain,
}
impl StopReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Limit => "limit",
            Self::Budget => "budget",
            Self::Lease => "lease",
            Self::Complete => "complete",
            Self::Uncertain => "uncertain",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CleanupOutcome {
    pub deleted_objects: u64,
    pub deleted_bytes: u64,
    pub stop_reason: StopReason,
}
impl CleanupOutcome {
    fn stopped(stop_reason: StopReason) -> Self {
        Self {
            deleted_objects: 0,
            deleted_bytes: 0,
            stop_reason,
        }
    }
    /// Every number is a decimal string, which is what the renderer reads.
    pub(crate) fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "deletedObjects": self.deleted_objects.to_string(),
            "deletedBytes": self.deleted_bytes.to_string(),
            "stopReason": self.stop_reason.as_str(),
        })
    }
}

/// One reading of everything a run protects that is not a lease. The same
/// reading is taken again once the marker is confirmed, and the two are
/// compared by identity rather than by count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ObservedRoots {
    /// The state the head names. A backup-only repository has none.
    pub head: Option<RemoteObject>,
    /// Every backup point the collection holds, whatever this run decided
    /// about it.
    pub point_ids: BTreeSet<String>,
    pub kept_points: Vec<RemoteObject>,
    pub kept_bundles: Vec<RemoteObject>,
    pub dropped_points: Vec<RemoteObject>,
}
impl ObservedRoots {
    /// What the final check compares. A point that appeared and a point that
    /// went both count as a change, and so does a different head.
    fn unchanged_from(&self, earlier: &Self) -> bool {
        self.point_ids == earlier.point_ids
            && self.head.as_ref().map(|object| &object.object_id)
                == earlier.head.as_ref().map(|object| &object.object_id)
    }
}

/// What an unfinished job on this device holds. These are roots of this run
/// whatever the repository says, because a job that never resolved still owns
/// what it uploaded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct JobRoots {
    pub objects: Vec<ObjectReceipt>,
    pub snapshot_ids: BTreeSet<String>,
}

/// The repository as a run reads it, apart from the lease collection.
pub(crate) trait RepositoryView: Sync {
    fn roots<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, ObservedRoots>;
    /// Everything the snapshot collection answers, read to the end.
    fn snapshots<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, Vec<ObjectReceipt>>;
    fn job_roots(&self) -> Result<JobRoots>;
}

pub(crate) struct CleanupRequest<'a> {
    pub job_id: &'a str,
    pub capabilities: &'a Capabilities,
    pub limits: CleanupLimits,
    pub now_ms: u64,
}

fn unsupported() -> ProviderError {
    ProviderError::new(ErrorKind::Unsupported)
}

/// True while no other device is using the repository. A marker of this
/// device's own attempt is excluded; every other marker, including one an
/// interrupted run left, means someone may still be removing.
async fn repository_is_quiet(
    ctx: &LeaseContext<'_>,
    own: Option<&LeaseHandle>,
    cancel: &Cancellation,
) -> Result<bool> {
    let Some(survey) = leases::survey_single_page(ctx, cancel).await? else {
        return Ok(false);
    };
    Ok(survey.foreign_work().is_empty()
        && survey
            .blocking_markers(own.map(|handle| &handle.locator))
            .is_empty())
}

/// Removes the marker of an attempt that sent nothing, or that saw every
/// request it sent answered. A marker that cannot go stays and the run says so.
async fn withdraw(ctx: &LeaseContext<'_>, marker: &LeaseHandle) -> StopReason {
    match leases::clear_marker(ctx, marker).await {
        Ok(()) => StopReason::Lease,
        Err(_) => StopReason::Uncertain,
    }
}

fn record(
    ctx: &LeaseContext<'_>,
    outcome: &CleanupOutcome,
    now_ms: u64,
) -> Result<()> {
    GcStore::open(ctx.root)?.record_cleanup_run(
        ctx.connection_id,
        now_ms,
        outcome.stop_reason.as_str(),
        outcome.deleted_objects,
        outcome.deleted_bytes,
    )
}

/// One cleanup, in the order the repository has to see it.
pub(crate) async fn run(
    ctx: &LeaseContext<'_>,
    request: &CleanupRequest<'_>,
    view: &dyn RepositoryView,
    source: &dyn DocumentSource,
    cancel: &Cancellation,
) -> Result<CleanupOutcome> {
    if request.limits.batch == 0 || request.limits.per_run == 0 {
        return Err(unsupported());
    }
    ctx.descriptor.validate().map_err(|_| unsupported())?;
    request.capabilities.require_cleanup()?;

    // Another device's job may be reading exactly what this run would decide is
    // unreachable, so nothing is read for the decision until the repository is
    // quiet.
    if !repository_is_quiet(ctx, None, cancel).await? {
        let outcome = CleanupOutcome::stopped(StopReason::Lease);
        record(ctx, &outcome, request.now_ms)?;
        return Ok(outcome);
    }

    let before = view.roots(cancel).await?;
    let listed = view.snapshots(cancel).await?;
    let jobs = view.job_roots()?;
    let mark = reachability::mark(
        MarkRequest {
            connection_id: ctx.connection_id,
            now_ms: request.now_ms,
            roots: Roots {
                head: before.head.clone(),
                kept_points: before.kept_points.clone(),
                kept_bundles: before.kept_bundles.clone(),
                job_objects: jobs.objects.clone(),
                job_snapshot_ids: jobs.snapshot_ids.clone(),
            },
            listed,
            dropped_points: before.dropped_points.clone(),
        },
        source,
        ctx.root,
        cancel,
    )
    .await?;
    // A list this device could no longer read cost it whatever it named, and a
    // document it could not open takes itself out of this run.
    if mark.list_recomputed {
        crate::nlog!(
            "error",
            "External storage cleanup list was unreadable and was computed again"
        );
    }
    if mark.skipped > 0 {
        crate::nlog!(
            "info",
            "External storage cleanup skipped {} unreadable documents",
            mark.skipped
        );
    }
    if mark.candidates.is_empty() {
        let outcome = CleanupOutcome::stopped(StopReason::Complete);
        record(ctx, &outcome, request.now_ms)?;
        return Ok(outcome);
    }

    // From here the repository has to show every other device that a removal is
    // running, before anything is read again and long before anything goes.
    let marker = leases::place_marker(ctx, request.job_id, request.now_ms, cancel).await?;
    let quiet = repository_is_quiet(ctx, Some(&marker), cancel).await;
    let after = match quiet {
        Ok(true) => view.roots(cancel).await,
        Ok(false) => {
            let outcome = CleanupOutcome::stopped(withdraw(ctx, &marker).await);
            record(ctx, &outcome, request.now_ms)?;
            return Ok(outcome);
        }
        Err(error) => {
            let _ = leases::clear_marker(ctx, &marker).await;
            return Err(error);
        }
    };
    // A publication that finished between the mark and the marker left the
    // leases empty but changed the roots, which is what this comparison is for.
    match after {
        Ok(after) if after.unchanged_from(&before) => {}
        Ok(_) => {
            let outcome = CleanupOutcome::stopped(withdraw(ctx, &marker).await);
            record(ctx, &outcome, request.now_ms)?;
            return Ok(outcome);
        }
        Err(error) => {
            let _ = leases::clear_marker(ctx, &marker).await;
            return Err(error);
        }
    }

    let mut outcome = CleanupOutcome {
        deleted_objects: 0,
        deleted_bytes: 0,
        stop_reason: StopReason::Complete,
    };
    let mut sent = 0usize;
    let mut cancelled = false;
    'run: for (index, batch) in mark.candidates.chunks(request.limits.batch).enumerate() {
        // The first batch is covered by the check above; every later one asks
        // again, because a publisher that entered meanwhile has to be let past.
        if index > 0
            && !repository_is_quiet(ctx, Some(&marker), cancel)
                .await
                .unwrap_or(false)
        {
            outcome.stop_reason = StopReason::Lease;
            break;
        }
        for entry in batch {
            if sent >= request.limits.per_run {
                outcome.stop_reason = StopReason::Limit;
                break 'run;
            }
            if cancel.check().is_err() {
                cancelled = true;
                break 'run;
            }
            // The row is written before the request, so an answer this device
            // never sees still names a request it cannot call finished.
            leases::note_delete_sent(ctx, &marker, &entry.locator, request.now_ms)?;
            sent += 1;
            match ctx
                .provider
                .delete_object(ctx.repository, &entry.locator, cancel)
                .await
            {
                Ok(()) => {
                    leases::note_delete_finished(ctx, &marker, &entry.locator)?;
                    GcStore::open(ctx.root)?
                        .mark_deletion_done(ctx.connection_id, &entry.locator)?;
                    outcome.deleted_objects += 1;
                    outcome.deleted_bytes =
                        outcome.deleted_bytes.saturating_add(entry.byte_length);
                }
                // No answer arrived, so this device cannot tell a request that
                // never left from one the repository is still running. It stops
                // and keeps the marker until the end of that request is known.
                Err(error) => {
                    outcome.stop_reason = match error.kind {
                        ErrorKind::RateLimited | ErrorKind::DailyQuotaExhausted => {
                            StopReason::Budget
                        }
                        _ => StopReason::Uncertain,
                    };
                    break 'run;
                }
            }
        }
    }
    // A cancelled run has targets left and did not yield to anyone, so the next
    // run may continue as soon as it is asked to.
    if cancelled && outcome.stop_reason == StopReason::Complete {
        outcome.stop_reason = StopReason::Limit;
    }

    match leases::clear_marker(ctx, &marker).await {
        Ok(()) => {}
        Err(_) => outcome.stop_reason = StopReason::Uncertain,
    }
    record(ctx, &outcome, request.now_ms)?;
    if cancelled && outcome.stop_reason != StopReason::Uncertain {
        return Err(ProviderError::new(ErrorKind::Cancelled));
    }
    Ok(outcome)
}

/// The repository behind one connection, read through the provider. Everything
/// here is a read; the removal itself stays in `run`.
pub(crate) struct ConnectedRepositoryView<'a> {
    pub connected: &'a ConnectedRepository,
    pub writer_id: &'a str,
    pub policy: RetentionPolicy,
    pub now_ms: u64,
    /// The job directory of every unfinished job on this connection.
    pub unfinished: Vec<(String, std::path::PathBuf)>,
}

impl ConnectedRepositoryView<'_> {
    async fn read_points(&self, cancel: &Cancellation) -> Result<Vec<control::ListedBackupPoint>> {
        let mut points = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            cancel.check()?;
            let page = control::list_backup_points_page(
                &self.connected.stored.descriptor,
                &self.connected.root_key,
                self.connected.provider.as_ref(),
                &self.connected.handle,
                cursor.as_deref(),
                100,
                cancel,
            )
            .await?;
            points.extend(page.points);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        Ok(points)
    }

    /// Where one bundle's material came from, as the bundle's own document
    /// declares it. The point that names a bundle does not say who made it.
    async fn bundle_sources(
        &self,
        points: &[control::ListedBackupPoint],
        cancel: &Cancellation,
    ) -> Result<BTreeMap<String, BundleSource>> {
        let mut sources = BTreeMap::new();
        for point in points {
            for bundle in point.document.bundles() {
                cancel.check()?;
                if sources.contains_key(&bundle.object_id) {
                    continue;
                }
                let view = control::read_snapshot_document(self.connected, bundle, cancel).await?;
                let source = match view.captured_by_device {
                    Some(writer_id) => BundleSource::Device { writer_id },
                    None => BundleSource::SyncState {
                        commit_id: view.snapshot_id,
                    },
                };
                sources.insert(bundle.object_id.clone(), source);
            }
        }
        Ok(sources)
    }
}

impl RepositoryView for ConnectedRepositoryView<'_> {
    fn roots<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, ObservedRoots> {
        Box::pin(async move {
            let head = control::read_head(
                self.connected.provider.as_ref(),
                &self.connected.handle,
                &self.connected.stored.descriptor,
                &self.connected.root_key,
                None,
                cancel,
            )
            .await?
            .map(|observed| observed.document.state);
            let points = self.read_points(cancel).await?;
            let sources = self.bundle_sources(&points, cancel).await?;
            let decided: Vec<RetentionPoint> = points
                .iter()
                .map(|point| RetentionPoint {
                    point_id: point.document.point_id.clone(),
                    kind: point.document.kind,
                    created_at_ms: point.document.created_at_ms,
                    bundles: point
                        .document
                        .bundles()
                        .into_iter()
                        .filter_map(|bundle| {
                            sources.get(&bundle.object_id).map(|source| RetentionBundle {
                                object_id: bundle.object_id.clone(),
                                source: source.clone(),
                            })
                        })
                        .collect(),
                })
                .collect();
            let decision =
                decide_retention(&decided, self.writer_id, self.policy, self.now_ms);
            let removed: BTreeSet<&str> = decision.remove.iter().map(String::as_str).collect();
            let mut roots = ObservedRoots {
                head,
                ..ObservedRoots::default()
            };
            for point in &points {
                roots.point_ids.insert(point.document.point_id.clone());
                if removed.contains(point.document.point_id.as_str()) {
                    roots.dropped_points.push(point.reference.clone());
                    continue;
                }
                roots.kept_points.push(point.reference.clone());
                roots
                    .kept_bundles
                    .extend(point.document.bundles().into_iter().cloned());
            }
            Ok(roots)
        })
    }

    fn snapshots<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, Vec<ObjectReceipt>> {
        Box::pin(async move {
            let mut listed = Vec::new();
            let mut cursor: Option<String> = None;
            loop {
                cancel.check()?;
                let page = self
                    .connected
                    .provider
                    .list_objects(
                        &self.connected.handle,
                        Collection::Snapshots,
                        cursor.as_deref(),
                        100,
                        cancel,
                    )
                    .await?;
                listed.extend(page.objects);
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            Ok(listed)
        })
    }

    fn job_roots(&self) -> Result<JobRoots> {
        let mut roots = JobRoots::default();
        for (job_id, directory) in &self.unfinished {
            roots.snapshot_ids.insert(job_id.clone());
            // A job that never reached its first upload has no journal, which
            // is not a reason to refuse the run.
            if let Ok(receipts) = TransferJournal::uploaded(directory, job_id) {
                roots.objects.extend(receipts);
            }
        }
        Ok(roots)
    }
}

/// The documents a mark follows, read through the provider.
pub(crate) struct ConnectedDocuments<'a> {
    pub connected: &'a ConnectedRepository,
    pub cancel: &'a Cancellation,
}

impl ConnectedDocuments<'_> {
    fn node(&self, view: &control::SnapshotView) -> Result<DocumentNode> {
        let mut references = Vec::new();
        for stored in reachability::document_references(view) {
            references.push(RemoteObject::from_stored(stored, &self.connected.handle)?);
        }
        Ok(DocumentNode {
            snapshot_id: view.snapshot_id.clone(),
            parent_snapshot_id: view.parent_snapshot_id.clone(),
            references,
        })
    }
}

impl DocumentSource for ConnectedDocuments<'_> {
    fn document<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, DocumentNode> {
        Box::pin(async move {
            let view =
                control::read_snapshot_document(self.connected, object, self.cancel).await?;
            self.node(&view)
        })
    }
    fn listed<'a>(
        &'a self,
        receipt: &'a ObjectReceipt,
    ) -> ProviderFuture<'a, (RemoteObject, DocumentNode)> {
        Box::pin(async move {
            let (object, view) =
                control::open_listed_snapshot(self.connected, receipt.clone(), self.cancel).await?;
            let node = self.node(&view)?;
            Ok((object, node))
        })
    }
    fn catalog<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Vec<RemoteObject>> {
        Box::pin(async move {
            control::read_catalog_children(self.connected, object, self.cancel).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::{
        contract::{
            lease_object_id, LeaseKind, ObjectRole, Provider, RemoteLocator, RepositoryHandle,
        },
        fake::{self, DeleteFault, FakeProvider},
        gc_store::{CommittedDeletion, LeaseIntent, LeaseState},
    };
    use risunest_external_storage_format::format::{Descriptor, Strategy};
    use std::{
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    const DAY: u64 = 24 * 60 * 60 * 1000;
    const NOW: u64 = 1_000 * DAY;
    const CONNECTION: &str = "connection";

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn object(id: &str, role: ObjectRole, byte_length: u64) -> RemoteObject {
        RemoteObject {
            repository_id: "synthetic-repository".into(),
            object_id: id.into(),
            role,
            receipt: ObjectReceipt {
                locator: RemoteLocator {
                    connection_identity: "synthetic-account/root".into(),
                    collection: None,
                    object: id.into(),
                },
                byte_length,
                version: None,
                checksum: None,
                complete: true,
            },
            ciphertext_sha256: "11".repeat(32),
            plaintext_length: byte_length,
            plaintext_sha256: "22".repeat(32),
        }
    }

    /// A repository this test drives directly: every reading is arranged, and
    /// the readings a run takes twice can differ between the two.
    #[derive(Default)]
    struct Scripted {
        readings: Vec<ObservedRoots>,
        snapshots: Vec<ObjectReceipt>,
        jobs: JobRoots,
        documents: BTreeMap<String, DocumentNode>,
        catalogs: BTreeMap<String, Vec<RemoteObject>>,
        failing: BTreeSet<String>,
        taken: AtomicUsize,
        opened: Mutex<Vec<String>>,
    }
    impl Scripted {
        fn read(&self, id: &str) -> Result<()> {
            self.opened.lock().unwrap().push(id.to_owned());
            if self.failing.contains(id) {
                return Err(ProviderError::new(ErrorKind::Transient));
            }
            Ok(())
        }
        fn node(&self, id: &str) -> Result<DocumentNode> {
            self.read(id)?;
            self.documents
                .get(id)
                .cloned()
                .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))
        }
        fn documents_read(&self) -> usize {
            self.opened.lock().unwrap().len()
        }
    }
    impl RepositoryView for Scripted {
        fn roots<'a>(&'a self, _: &'a Cancellation) -> ProviderFuture<'a, ObservedRoots> {
            Box::pin(async move {
                let index = self.taken.fetch_add(1, Ordering::SeqCst);
                self.readings
                    .get(index)
                    .or_else(|| self.readings.last())
                    .cloned()
                    .ok_or_else(|| ProviderError::new(ErrorKind::Transient))
            })
        }
        fn snapshots<'a>(&'a self, _: &'a Cancellation) -> ProviderFuture<'a, Vec<ObjectReceipt>> {
            Box::pin(async move { Ok(self.snapshots.clone()) })
        }
        fn job_roots(&self) -> Result<JobRoots> {
            Ok(self.jobs.clone())
        }
    }
    impl DocumentSource for Scripted {
        fn document<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, DocumentNode> {
            Box::pin(async move { self.node(&object.object_id) })
        }
        fn listed<'a>(
            &'a self,
            receipt: &'a ObjectReceipt,
        ) -> ProviderFuture<'a, (RemoteObject, DocumentNode)> {
            Box::pin(async move {
                let node = self.node(&receipt.locator.object)?;
                let mut object = object(
                    &receipt.locator.object,
                    ObjectRole::SyncState,
                    receipt.byte_length,
                );
                object.receipt = receipt.clone();
                Ok((object, node))
            })
        }
        fn catalog<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Vec<RemoteObject>> {
            Box::pin(async move {
                self.read(&object.object_id)?;
                self.catalogs
                    .get(&object.object_id)
                    .cloned()
                    .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))
            })
        }
    }

    struct Harness {
        _directory: tempfile::TempDir,
        root: PathBuf,
        provider: FakeProvider,
        repository: RepositoryHandle,
        descriptor: Descriptor,
        root_key: [u8; 32],
        capabilities: Capabilities,
    }
    impl Harness {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().to_path_buf();
            Self {
                _directory: directory,
                root,
                provider: FakeProvider::new(true),
                repository: fake::repository(),
                descriptor: Descriptor::new("synthetic-descriptor".into(), Some(Strategy::Cas))
                    .unwrap(),
                root_key: [7; 32],
                capabilities: fake::capabilities(true),
            }
        }
        fn context(&self) -> LeaseContext<'_> {
            LeaseContext {
                root: &self.root,
                connection_id: CONNECTION,
                writer_id: "writer",
                descriptor: &self.descriptor,
                root_key: &self.root_key,
                provider: &self.provider,
                repository: &self.repository,
            }
        }
        fn request<'a>(&'a self, job_id: &'a str, limits: CleanupLimits) -> CleanupRequest<'a> {
            CleanupRequest {
                job_id,
                capabilities: &self.capabilities,
                limits,
                now_ms: NOW,
            }
        }
        fn store(&self) -> GcStore {
            GcStore::open(&self.root).unwrap()
        }
        /// Puts one object in the repository so a removal has something to
        /// answer for.
        fn place(&self, object: &RemoteObject) {
            self.provider.seed(
                &object.object_id,
                object.role,
                vec![0; object.receipt.byte_length as usize],
            );
        }
        /// A lease another device left behind. Only its name is readable from
        /// here, which is all a survey uses.
        fn foreign(&self, kind: LeaseKind, tag: &str) -> String {
            let object = lease_object_id(kind, tag).unwrap();
            self.provider
                .seed(&object, ObjectRole::Lease, b"foreign".to_vec());
            object
        }
        /// A lease this device placed, recorded the way a confirmed
        /// registration records one. A survey answers `mine` for it, so only
        /// the page count can make it matter.
        fn own_lease(&self, tag: &str) {
            let object = lease_object_id(LeaseKind::Work, tag).unwrap();
            self.provider
                .seed(&object, ObjectRole::Lease, b"mine".to_vec());
            self.store()
                .put_lease_intent(
                    CONNECTION,
                    &LeaseIntent {
                        locator: RemoteLocator {
                            connection_identity: self.repository.connection_identity.clone(),
                            collection: None,
                            object,
                        },
                        kind: LeaseKind::Work,
                        job_id: "other".into(),
                        seq: 0,
                        bytes: b"mine".to_vec(),
                        state: LeaseState::Confirmed,
                        created_at_ms: NOW,
                    },
                )
                .unwrap();
        }
        fn markers(&self) -> Vec<String> {
            self.provider
                .state
                .lock()
                .unwrap()
                .objects
                .keys()
                .filter(|name| name.starts_with("deleting-"))
                .cloned()
                .collect()
        }
        fn committed(&self) -> Vec<CommittedDeletion> {
            self.store().committed_deletions(CONNECTION).unwrap()
        }
        fn remaining(&self) -> Vec<String> {
            self.committed()
                .into_iter()
                .filter(|entry| !entry.done)
                .map(|entry| entry.locator.object)
                .collect()
        }
    }

    fn tag(index: u8) -> String {
        format!("{index:x}").repeat(32)
    }

    /// One head with a live subtree and one displaced state past the grace
    /// window with a subtree of its own.
    struct Library {
        head: RemoteObject,
        live_catalog: RemoteObject,
        live_pack: RemoteObject,
        stale: RemoteObject,
        stale_catalog: RemoteObject,
        stale_pack: RemoteObject,
    }
    impl Library {
        fn install(harness: &Harness) -> (Self, Scripted) {
            let head = object("snapshot-head", ObjectRole::SyncState, 10);
            let live_catalog = object("catalog-live", ObjectRole::Catalog, 20);
            let live_pack = object("pack-live", ObjectRole::Pack, 30);
            let stale = object("snapshot-stale", ObjectRole::SyncState, 40);
            let stale_catalog = object("catalog-stale", ObjectRole::Catalog, 50);
            let stale_pack = object("pack-stale", ObjectRole::Pack, 60);
            for item in [
                &head,
                &live_catalog,
                &live_pack,
                &stale,
                &stale_catalog,
                &stale_pack,
            ] {
                harness.place(item);
            }
            // The displaced state has been visible to this device for longer
            // than the grace window, which is the only thing that makes it a
            // candidate.
            harness
                .store()
                .record_observations(
                    CONNECTION,
                    &BTreeSet::from(["snapshot-stale".to_owned()]),
                    NOW - 8 * DAY,
                )
                .unwrap();
            let library = Self {
                head,
                live_catalog,
                live_pack,
                stale,
                stale_catalog,
                stale_pack,
            };
            let scripted = library.script();
            (library, scripted)
        }

        /// The same repository as an arrangement, without placing anything.
        /// A later run reads it again after an earlier one removed objects.
        fn script(&self) -> Scripted {
            Scripted {
                readings: vec![ObservedRoots {
                    head: Some(self.head.clone()),
                    ..ObservedRoots::default()
                }],
                snapshots: vec![self.head.receipt.clone(), self.stale.receipt.clone()],
                documents: BTreeMap::from([
                    (
                        "snapshot-head".to_owned(),
                        DocumentNode {
                            snapshot_id: "snapshot-head".into(),
                            parent_snapshot_id: None,
                            references: vec![self.live_catalog.clone()],
                        },
                    ),
                    (
                        "snapshot-stale".to_owned(),
                        DocumentNode {
                            snapshot_id: "snapshot-stale".into(),
                            parent_snapshot_id: None,
                            references: vec![self.stale_catalog.clone()],
                        },
                    ),
                ]),
                catalogs: BTreeMap::from([
                    ("catalog-live".to_owned(), vec![self.live_pack.clone()]),
                    ("catalog-stale".to_owned(), vec![self.stale_pack.clone()]),
                ]),
                ..Scripted::default()
            }
        }
    }

    fn removed(harness: &Harness, library: &Library) -> Vec<bool> {
        [&library.stale, &library.stale_catalog, &library.stale_pack]
            .iter()
            .map(|object| !harness.provider.holds(&object.object_id))
            .collect()
    }

    /// The publication the current head names is never a candidate, whatever
    /// else this run removes.
    fn live_material_survived(harness: &Harness, library: &Library) {
        assert!(harness.provider.holds(&library.head.object_id));
        assert!(harness.provider.holds("catalog-live"));
        assert!(harness.provider.holds(&library.live_pack.object_id));
    }

    /// Invariants GC30 and GC19. Another device's lease stops the run before
    /// any document is read, so nothing is decided and no marker is placed.
    #[test]
    fn another_device_s_lease_stops_the_run_before_the_mark_starts() {
        let harness = Harness::new();
        let (_library, scripted) = Library::install(&harness);
        harness.foreign(LeaseKind::Work, &tag(1));
        let outcome = runtime()
            .block_on(run(
                &harness.context(),
                &harness.request("job", CleanupLimits::default()),
                &scripted,
                &scripted,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Lease);
        assert_eq!(outcome.deleted_objects, 0);
        assert_eq!(scripted.documents_read(), 0);
        assert!(harness.markers().is_empty());
        assert!(harness.committed().is_empty());
    }

    /// Invariant GC19. Another device's cleanup counts the same as its work.
    #[test]
    fn another_device_s_cleanup_lease_also_stops_the_run() {
        let harness = Harness::new();
        let (_library, scripted) = Library::install(&harness);
        harness.foreign(LeaseKind::Cleanup, &tag(2));
        let outcome = runtime()
            .block_on(run(
                &harness.context(),
                &harness.request("job", CleanupLimits::default()),
                &scripted,
                &scripted,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Lease);
        assert_eq!(scripted.documents_read(), 0);
    }

    /// Invariants GC3 and GC19. A lease collection that does not fit one page
    /// cannot show that nobody is removing, because no provider documents that
    /// a cursor walk keeps every object visible. The leases here are this
    /// device's own, so only the page count can stop the run.
    #[test]
    fn a_lease_collection_that_needs_a_second_page_defers_the_run() {
        for (count, expected, reads) in [
            (99usize, StopReason::Complete, true),
            (101, StopReason::Lease, false),
        ] {
            let harness = Harness::new();
            let (_library, scripted) = Library::install(&harness);
            for index in 0..count {
                harness.own_lease(&format!("{index:032x}"));
            }
            let outcome = runtime()
                .block_on(run(
                    &harness.context(),
                    &harness.request("job", CleanupLimits::default()),
                    &scripted,
                    &scripted,
                    &Cancellation::default(),
                ))
                .unwrap();
            assert_eq!(outcome.stop_reason, expected);
            assert_eq!(scripted.documents_read() > 0, reads);
        }
    }

    /// Invariant GC6. A run that stops at its bound has removed the document
    /// before the fragments under it, a later run finishes the list, and a
    /// target that is already gone answers the same as one removed now.
    #[test]
    fn a_run_removes_parents_first_and_a_later_run_finishes_the_list() {
        let harness = Harness::new();
        let (library, scripted) = Library::install(&harness);
        let single = CleanupLimits {
            batch: 25,
            per_run: 1,
        };
        let first = runtime()
            .block_on(run(
                &harness.context(),
                &harness.request("first", single),
                &scripted,
                &scripted,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(first.stop_reason, StopReason::Limit);
        assert_eq!(first.deleted_objects, 1);
        assert_eq!(first.deleted_bytes, library.stale.receipt.byte_length);
        assert_eq!(removed(&harness, &library), [true, false, false]);
        assert_eq!(
            harness.remaining(),
            ["catalog-stale".to_owned(), "pack-stale".to_owned()]
        );
        assert!(harness.markers().is_empty());
        live_material_survived(&harness, &library);

        // The document is gone, so the enumeration no longer answers with it
        // and only the committed list can still name what it hid.
        let mut resumed = library.script();
        resumed.snapshots = vec![library.head.receipt.clone()];
        let second = runtime()
            .block_on(run(
                &harness.context(),
                &harness.request("second", CleanupLimits::default()),
                &resumed,
                &resumed,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(second.stop_reason, StopReason::Complete);
        assert_eq!(second.deleted_objects, 2);
        assert_eq!(removed(&harness, &library), [true, true, true]);
        assert!(harness.remaining().is_empty());
        live_material_survived(&harness, &library);

        // Removing the same target twice succeeds, which is what lets an
        // interrupted run be restarted as it is.
        harness
            .store()
            .replace_deletions(
                CONNECTION,
                &[CommittedDeletion {
                    locator: library.stale_pack.receipt.locator.clone(),
                    role: ObjectRole::Pack,
                    byte_length: library.stale_pack.receipt.byte_length,
                    decided_at_ms: NOW,
                    done: false,
                }],
            )
            .unwrap();
        let again = runtime()
            .block_on(run(
                &harness.context(),
                &harness.request("third", CleanupLimits::default()),
                &resumed,
                &resumed,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(again.stop_reason, StopReason::Complete);
        assert_eq!(again.deleted_objects, 1);
    }

    /// Invariants GC18 and GC32. A publication that finished between the mark
    /// and the marker left no lease behind, and the reading taken after the
    /// marker is what catches it.
    #[test]
    fn a_publication_that_finished_before_the_marker_is_caught_by_the_final_reading() {
        let harness = Harness::new();
        let (library, mut scripted) = Library::install(&harness);
        let next = object("snapshot-next", ObjectRole::SyncState, 11);
        scripted.readings = vec![
            ObservedRoots {
                head: Some(library.head.clone()),
                ..ObservedRoots::default()
            },
            ObservedRoots {
                head: Some(next),
                ..ObservedRoots::default()
            },
        ];
        let outcome = runtime()
            .block_on(run(
                &harness.context(),
                &harness.request("job", CleanupLimits::default()),
                &scripted,
                &scripted,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Lease);
        assert_eq!(outcome.deleted_objects, 0);
        assert_eq!(removed(&harness, &library), [false, false, false]);
        assert!(harness.markers().is_empty());
    }

    /// Invariants GC18 and GC32. A backup-only repository has no head, so the
    /// point identifiers are the whole comparison, and a point that appeared
    /// and one that went both count.
    #[test]
    fn a_backup_only_repository_compares_the_point_identifiers_exactly() {
        for (before, after) in [
            (["point-a"].as_slice(), ["point-a", "point-b"].as_slice()),
            (["point-a", "point-b"].as_slice(), ["point-a"].as_slice()),
        ] {
            let harness = Harness::new();
            let (library, mut scripted) = Library::install(&harness);
            let reading = |ids: &[&str]| ObservedRoots {
                head: None,
                point_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
                ..ObservedRoots::default()
            };
            scripted.readings = vec![reading(before), reading(after)];
            let outcome = runtime()
                .block_on(run(
                    &harness.context(),
                    &harness.request("job", CleanupLimits::default()),
                    &scripted,
                    &scripted,
                    &Cancellation::default(),
                ))
                .unwrap();
            assert_eq!(outcome.stop_reason, StopReason::Lease);
            assert_eq!(outcome.deleted_objects, 0);
            assert_eq!(removed(&harness, &library), [false, false, false]);
            assert!(harness.markers().is_empty());
        }
    }

    /// Invariant GC33. A publisher that took its lease after the marker was
    /// confirmed is seen by the final reading, and nothing is removed.
    #[test]
    fn a_lease_taken_after_the_marker_stops_the_run_before_the_first_removal() {
        let harness = Harness::new();
        let (library, scripted) = Library::install(&harness);
        harness.provider.seed_after_list(
            1,
            &lease_object_id(LeaseKind::Work, &tag(4)).unwrap(),
            ObjectRole::Lease,
            b"foreign".to_vec(),
        );
        let outcome = runtime()
            .block_on(run(
                &harness.context(),
                &harness.request("job", CleanupLimits::default()),
                &scripted,
                &scripted,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Lease);
        assert_eq!(outcome.deleted_objects, 0);
        assert_eq!(removed(&harness, &library), [false, false, false]);
        assert!(harness.markers().is_empty());
        // The decision itself is kept: the next run starts from it.
        assert_eq!(harness.remaining().len(), 3);
    }

    /// Invariant GC19. A lease that appears once removals have started stops
    /// the next batch, and the marker goes only because every request sent had
    /// already answered.
    #[test]
    fn a_lease_that_appears_between_batches_stops_the_next_one() {
        let harness = Harness::new();
        let (library, scripted) = Library::install(&harness);
        harness.provider.seed_after_delete(
            1,
            &lease_object_id(LeaseKind::Work, &tag(5)).unwrap(),
            ObjectRole::Lease,
            b"foreign".to_vec(),
        );
        let outcome = runtime()
            .block_on(run(
                &harness.context(),
                &harness.request(
                    "job",
                    CleanupLimits {
                        batch: 1,
                        per_run: 200,
                    },
                ),
                &scripted,
                &scripted,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Lease);
        assert_eq!(outcome.deleted_objects, 1);
        assert_eq!(removed(&harness, &library), [true, false, false]);
        assert!(harness.markers().is_empty());
        assert_eq!(
            harness.remaining(),
            ["catalog-stale".to_owned(), "pack-stale".to_owned()]
        );
    }

    /// Invariant GC34. An answer that never arrives leaves a request this
    /// device cannot call finished, so the marker stays and a later run is the
    /// one that waits, not the one that guesses.
    #[test]
    fn an_answer_that_never_arrives_keeps_the_marker_in_place() {
        let harness = Harness::new();
        let (library, scripted) = Library::install(&harness);
        harness
            .provider
            .fail_delete(&library.stale.object_id, DeleteFault::Unanswered);
        let cancel = Cancellation::default();
        let outcome = runtime().block_on(async {
            let stop = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                stop.cancel();
            });
            run(
                &harness.context(),
                &harness.request("job", CleanupLimits::default()),
                &scripted,
                &scripted,
                &cancel,
            )
            .await
        });
        let outcome = outcome.unwrap();
        assert_eq!(outcome.stop_reason, StopReason::Uncertain);
        assert_eq!(outcome.deleted_objects, 0);
        assert_eq!(harness.markers().len(), 1);
        assert_eq!(
            harness
                .store()
                .unfinished_delete_requests(CONNECTION, None)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(harness.remaining().len(), 3);
    }

    /// Invariant GC34. A removal the repository applied but never answered for
    /// is not finished, and the run that follows waits on the marker instead of
    /// reading its own success as the end of the earlier request.
    #[test]
    fn an_applied_removal_with_a_lost_answer_is_not_treated_as_finished() {
        let harness = Harness::new();
        let (library, scripted) = Library::install(&harness);
        harness
            .provider
            .fail_delete(&library.stale.object_id, DeleteFault::AppliedThenLost);
        let first = runtime()
            .block_on(run(
                &harness.context(),
                &harness.request("first", CleanupLimits::default()),
                &scripted,
                &scripted,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(first.stop_reason, StopReason::Uncertain);
        assert_eq!(first.deleted_objects, 0);
        // The object is gone remotely, which this device has no way to know.
        assert!(!harness.provider.holds(&library.stale.object_id));
        assert_eq!(harness.markers().len(), 1);
        let outstanding = harness
            .store()
            .unfinished_delete_requests(CONNECTION, None)
            .unwrap();
        assert_eq!(outstanding.len(), 1);
        assert!(harness
            .committed()
            .iter()
            .all(|entry| !entry.done || entry.locator.object != library.stale.object_id));

        let mut resumed = library.script();
        resumed.snapshots = vec![library.head.receipt.clone()];
        let second = runtime()
            .block_on(run(
                &harness.context(),
                &harness.request("second", CleanupLimits::default()),
                &resumed,
                &resumed,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(second.stop_reason, StopReason::Lease);
        assert_eq!(second.deleted_objects, 0);
        assert_eq!(resumed.documents_read(), 0);
        assert_eq!(harness.markers().len(), 1);
        assert_eq!(
            harness
                .store()
                .unfinished_delete_requests(CONNECTION, None)
                .unwrap(),
            outstanding
        );
        assert!(harness.provider.holds(&library.stale_pack.object_id));
    }

    /// Invariant GC17. Without every evidence the removal path needs, no marker
    /// and no removal request is made.
    #[test]
    fn a_repository_without_the_evidence_removes_nothing() {
        let mut harness = Harness::new();
        harness.capabilities = fake::capabilities_without_cleanup(true);
        let (library, scripted) = Library::install(&harness);
        let error = runtime()
            .block_on(run(
                &harness.context(),
                &harness.request("job", CleanupLimits::default()),
                &scripted,
                &scripted,
                &Cancellation::default(),
            ))
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
        assert_eq!(removed(&harness, &library), [false, false, false]);
        assert!(harness.markers().is_empty());
        assert_eq!(scripted.documents_read(), 0);
    }
}
