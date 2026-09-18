//! Bounded removal from a fresh, authenticated reachability observation.
//! Every execution owns finite protection and discards its remaining decision
//! when roots, clocks, permissions or remote request outcomes become uncertain.
use super::{
    connection::{decide_retention, RetentionBundle, RetentionPoint, RetentionPolicy},
    connection_commands::ConnectedRepository,
    contract::{
        Cancellation, Collection, ErrorKind, LeaseKind, ObjectIntent, ObjectReceipt, ObjectRole,
        ProviderError, ProviderFuture, RepositoryHandle, Result,
    },
    control,
    gc_store::{locator_key, GcStore},
    journal::TransferJournal,
    leases::{self, Admission, ClockReading, LeaseContext, LeaseOwner, PageTracker},
    packaging::{self, RemoteObject},
    reachability::{self, DocumentNode, DocumentSource, MarkRequest, RetiredPoint, Roots},
};
use risunest_external_storage_format::control::BundleSource;
use std::{collections::{BTreeMap, BTreeSet}, path::Path, time::{Duration, Instant}};

#[derive(Clone, Copy, Debug)]
pub(crate) struct CleanupLimits {
    pub batch: usize,
    pub per_run: usize,
}
impl Default for CleanupLimits {
    fn default() -> Self { Self { batch: 25, per_run: 200 } }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopReason {
    Limit,
    Budget,
    Lease,
    Clock,
    RootsChanged,
    Unsupported,
    Complete,
    Uncertain,
}
impl StopReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Limit => "limit",
            Self::Budget => "budget",
            Self::Lease => "lease",
            Self::Clock => "clock",
            Self::RootsChanged => "roots-changed",
            Self::Unsupported => "unsupported",
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
        Self { deleted_objects: 0, deleted_bytes: 0, stop_reason }
    }
    pub(crate) fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "deletedObjects": self.deleted_objects.to_string(),
            "deletedBytes": self.deleted_bytes.to_string(),
            "stopReason": self.stop_reason.as_str(),
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ObservedRoots {
    pub head: Option<RemoteObject>,
    pub points: BTreeMap<String, RemoteObject>,
    pub kept_points: Vec<RemoteObject>,
    pub kept_bundles: Vec<RemoteObject>,
    pub retired_points: Vec<RetiredPoint>,
}
impl ObservedRoots {
    fn identities(&self, repository: &RepositoryHandle) -> Result<BTreeMap<String, String>> {
        let mut identities = BTreeMap::new();
        if let Some(head) = &self.head {
            identities.insert("head".into(), reachability::object_identity(head, repository)?);
        }
        for (id, point) in &self.points {
            identities.insert(format!("point/{id}"), reachability::object_identity(point, repository)?);
        }
        for (kind, objects) in [("kept-point", &self.kept_points), ("kept-bundle", &self.kept_bundles)] {
            for object in objects {
                let key = format!("{kind}/{}", locator_key(&object.receipt.locator)?);
                let identity = reachability::object_identity(object, repository)?;
                if identities.insert(key, identity.clone()).is_some_and(|previous| previous != identity) {
                    return Err(ProviderError::new(ErrorKind::Corrupt));
                }
            }
        }
        for point in &self.retired_points {
            let prefix = format!("retired/{}", locator_key(&point.point.receipt.locator)?);
            identities.insert(prefix.clone(), reachability::object_identity(&point.point, repository)?);
            for bundle in &point.bundles {
                let key = format!("{prefix}/{}", locator_key(&bundle.receipt.locator)?);
                identities.insert(key, reachability::object_identity(bundle, repository)?);
            }
        }
        Ok(identities)
    }
    fn unchanged_from(&self, earlier: &Self, repository: &RepositoryHandle) -> Result<bool> {
        Ok(self.identities(repository)? == earlier.identities(repository)?)
    }
    fn removed(&mut self, object: &RemoteObject) {
        if object.role == ObjectRole::BackupPoint {
            self.points.retain(|_, point| point.receipt.locator != object.receipt.locator);
            self.retired_points.retain(|point| point.point.receipt.locator != object.receipt.locator);
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct JobRoots {
    pub objects: Vec<ObjectReceipt>,
    pub references: Vec<RemoteObject>,
    pub snapshot_ids: BTreeSet<String>,
}

pub(crate) trait RepositoryView: Sync {
    fn roots<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, ObservedRoots>;
    fn snapshots<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, Vec<ObjectReceipt>>;
    fn job_roots(&self) -> Result<JobRoots>;
    fn known_objects(&self) -> Result<Vec<RemoteObject>>;
}
pub(crate) struct CleanupRequest<'a> {
    pub job_id: &'a str,
    /// Actual list, lease and synchronous delete support from the provider.
    pub cleanup_supported: bool,
    /// A fresh, usable HTTP Date observation for this connection, collected no
    /// earlier than the instant passed here. Another account cannot satisfy it.
    pub connection_time: &'a (dyn Fn(Instant) -> Result<bool> + Sync),
    pub limits: CleanupLimits,
}

fn current_limit(
    context: &LeaseContext<'_>, request: &CleanupRequest<'_>, time_not_before: Instant,
    owner: &LeaseOwner, started: ClockReading, cancel: &Cancellation,
) -> Result<Option<StopReason>> {
    cancel.check()?;
    let now = context.clock.reading();
    if !now.foreground || now.epoch != started.epoch { return Ok(Some(StopReason::Lease)); }
    let recent = Instant::now()
        .checked_sub(Duration::from_millis(leases::RENEW_AFTER_MS))
        .map_or(time_not_before, |recent| recent.max(time_not_before));
    if !now.trusted || !(request.connection_time)(recent)? {
        return Ok(Some(StopReason::Clock));
    }
    if now.monotonic_ms.checked_sub(started.monotonic_ms)
        .is_none_or(|elapsed| elapsed >= leases::CLEANUP_RUN_LIMIT_MS)
    {
        return Ok(Some(StopReason::Budget));
    }
    if owner.check_control(context, true).is_err() { return Ok(Some(StopReason::Lease)); }
    Ok(None)
}

async fn recheck(
    context: &LeaseContext<'_>, owner: &LeaseOwner, view: &dyn RepositoryView,
    before: &ObservedRoots, jobs: &JobRoots, cancel: &Cancellation,
) -> Result<Option<StopReason>> {
    if owner.recheck(context, cancel).await?.is_some() { return Ok(Some(StopReason::Lease)); }
    let after = view.roots(cancel).await?;
    if !after.unchanged_from(before, context.repository)? || view.job_roots()? != *jobs {
        return Ok(Some(StopReason::RootsChanged));
    }
    if owner.recheck(context, cancel).await?.is_some() { return Ok(Some(StopReason::Lease)); }
    Ok(None)
}

fn record(context: &LeaseContext<'_>, outcome: &CleanupOutcome) -> Result<()> {
    GcStore::open(context.root)?.record_cleanup_run(
        context.connection_id, context.clock.reading().wall_ms, outcome.stop_reason.as_str(),
        outcome.deleted_objects, outcome.deleted_bytes,
    )
}

pub(crate) async fn run(
    context: &LeaseContext<'_>, request: &CleanupRequest<'_>, view: &dyn RepositoryView,
    source: &dyn DocumentSource, cancel: &Cancellation,
) -> Result<CleanupOutcome> {
    cancel.check()?;
    if request.limits.batch == 0 || request.limits.per_run == 0 {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    context.descriptor.validate().map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    let initial = context.clock.reading();
    let refused = if !request.cleanup_supported || !context.protection_supported {
        Some(StopReason::Unsupported)
    } else if !initial.foreground {
        Some(StopReason::Lease)
    } else {
        None
    };
    // Opening a run invalidates the interval before its first remote reading.
    GcStore::open(context.root)?.begin_observation(context.connection_id)?;
    if let Some(reason) = refused {
        let outcome = CleanupOutcome::stopped(reason);
        record(context, &outcome)?;
        return Ok(outcome);
    }
    let time_not_before = Instant::now();
    let owner = match leases::admit(context, request.job_id, LeaseKind::Cleanup, cancel).await? {
        Admission::Admitted(owner) => owner,
        Admission::Yield { .. } => {
            let outcome = CleanupOutcome::stopped(StopReason::Lease);
            record(context, &outcome)?;
            return Ok(outcome);
        }
        Admission::UnsupportedProtection => {
            let outcome = CleanupOutcome::stopped(StopReason::Unsupported);
            record(context, &outcome)?;
            return Ok(outcome);
        }
    };
    let started = context.clock.reading();
    let connection_time = match (request.connection_time)(time_not_before) {
        Ok(available) => available,
        Err(error) => {
            owner.release_all(context).await;
            return Err(error);
        }
    };
    if !started.trusted || !connection_time {
        owner.release_all(context).await;
        let outcome = CleanupOutcome::stopped(StopReason::Clock);
        record(context, &outcome)?;
        return Ok(outcome);
    }
    let mut outcome = CleanupOutcome::stopped(StopReason::Uncertain);
    let result = owner.run(context, cancel, run_owned(
        context, request, time_not_before, &owner, started, view, source, cancel, &mut outcome,
    )).await;
    let bookkeeping = (|| {
        if result.is_err() || !matches!(outcome.stop_reason, StopReason::Complete | StopReason::Limit) {
            GcStore::open(context.root)?.invalidate_observations(context.connection_id)?;
        }
        record(context, &outcome)
    })();
    // A statistics failure must not turn 401, 429 or cancellation into a generic
    // storage error. The observation began invalid, so failure cannot grant age.
    result?;
    bookkeeping?;
    Ok(outcome)
}

async fn run_owned(
    context: &LeaseContext<'_>, request: &CleanupRequest<'_>, time_not_before: Instant,
    owner: &LeaseOwner, started: ClockReading, view: &dyn RepositoryView, source: &dyn DocumentSource,
    cancel: &Cancellation, outcome: &mut CleanupOutcome,
) -> Result<()> {
    if let Some(reason) = current_limit(context, request, time_not_before, owner, started, cancel)? {
        outcome.stop_reason = reason;
        return Ok(());
    }
    let mut before = view.roots(cancel).await?;
    let listed = view.snapshots(cancel).await?;
    let jobs = view.job_roots()?;
    let marked = reachability::mark(context.root, source, MarkRequest {
        connection_id: context.connection_id, repository: context.repository,
        format_repository_id: &context.descriptor.repository_id,
        now_ms: context.clock.reading().wall_ms,
        roots: Roots {
            head: before.head.clone(), kept_points: before.kept_points.clone(),
            kept_bundles: before.kept_bundles.clone(), job_objects: jobs.objects.clone(),
            job_references: jobs.references.clone(), job_snapshot_ids: jobs.snapshot_ids.clone(),
        },
        listed, known_objects: view.known_objects()?, retired_points: before.retired_points.clone(),
    }, cancel).await?;
    if let Some(reason) = current_limit(context, request, time_not_before, owner, started, cancel)? {
        outcome.stop_reason = reason;
        return Ok(());
    }
    if !marked.candidates.is_empty() {
        owner.renew_if_due(context, cancel).await?;
        owner.place_marker(context, cancel).await?;
    }
    if let Some(reason) = recheck(context, owner, view, &before, &jobs, cancel).await? {
        outcome.stop_reason = reason;
        return Ok(());
    }
    outcome.stop_reason = StopReason::Complete;
    let mut sent = 0;
    'batches: for batch in marked.candidates.chunks(request.limits.batch) {
        if let Some(reason) = current_limit(context, request, time_not_before, owner, started, cancel)? {
            outcome.stop_reason = reason;
            break;
        }
        owner.renew_if_due(context, cancel).await?;
        if let Some(reason) = recheck(context, owner, view, &before, &jobs, cancel).await? {
            outcome.stop_reason = reason;
            break;
        }
        for object in batch {
            if sent >= request.limits.per_run {
                outcome.stop_reason = StopReason::Limit;
                break 'batches;
            }
            if let Some(reason) = current_limit(context, request, time_not_before, owner, started, cancel)? {
                outcome.stop_reason = reason;
                break 'batches;
            }
            let key = locator_key(&object.receipt.locator)?;
            let Some(current) = source.probe(object).await? else {
                GcStore::open(context.root)?.forget_observation(context.connection_id, &key)?;
                before.removed(object);
                continue;
            };
            if current.locator != object.receipt.locator || !current.complete
                || current.byte_length != object.receipt.byte_length
            {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            // Probing can take time or suspend. Check again at the dispatch boundary.
            if let Some(reason) = current_limit(context, request, time_not_before, owner, started, cancel)? {
                outcome.stop_reason = reason;
                break 'batches;
            }
            owner.set_delete_in_flight(true);
            sent += 1;
            let removed = match leases::control_request(cancel, context.provider.delete_object(
                context.repository, &object.receipt.locator, cancel,
            )).await {
                Ok(()) => true,
                Err(error) if error.kind == ErrorKind::NotFound || error.http_status == Some(404) => true,
                Err(error) if error.http_status == Some(202) || error.kind == ErrorKind::Unsupported => {
                    outcome.stop_reason = StopReason::Unsupported;
                    break 'batches;
                }
                Err(error) if matches!(error.kind, ErrorKind::Transient | ErrorKind::Cancelled) => {
                    outcome.stop_reason = StopReason::Uncertain;
                    if error.kind == ErrorKind::Cancelled { return Err(error); }
                    break 'batches;
                }
                Err(error) => {
                    // An explicit refusal is not an unresolved remote deletion.
                    owner.set_delete_in_flight(false);
                    outcome.stop_reason = StopReason::Uncertain;
                    return Err(error);
                }
            };
            if removed {
                owner.set_delete_in_flight(false);
                GcStore::open(context.root)?.forget_observation(context.connection_id, &key)?;
                before.removed(object);
                outcome.deleted_objects = outcome.deleted_objects.checked_add(1)
                    .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
                outcome.deleted_bytes = outcome.deleted_bytes.checked_add(object.receipt.byte_length)
                    .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
            }
        }
    }
    if matches!(outcome.stop_reason, StopReason::Complete | StopReason::Limit) {
        if let Some(reason) = current_limit(context, request, time_not_before, owner, started, cancel)? {
            outcome.stop_reason = reason;
        } else if let Some(reason) = recheck(context, owner, view, &before, &jobs, cancel).await? {
            outcome.stop_reason = reason;
        } else if let Some(reason) = current_limit(context, request, time_not_before, owner, started, cancel)? {
            outcome.stop_reason = reason;
        } else {
            let store = GcStore::open(context.root)?;
            store.set_last_reachable_bytes(context.connection_id, marked.reachable_bytes)?;
            store.finish_observation(context.connection_id)?;
        }
    }
    Ok(())
}

pub(crate) struct ConnectedRepositoryView<'a> {
    pub connected: &'a ConnectedRepository,
    pub writer_id: &'a str,
    pub policy: RetentionPolicy,
    pub now_ms: u64,
    pub unfinished: Vec<UnfinishedJob>,
    pub cache_root: &'a Path,
}
pub(crate) struct UnfinishedJob {
    pub job_id: String,
    pub directory: std::path::PathBuf,
    pub snapshot_ids: BTreeSet<String>,
    /// Includes capture, conflict and export references held outside the journal.
    pub references: Vec<RemoteObject>,
}

fn job_roots_of(unfinished: &[UnfinishedJob]) -> Result<JobRoots> {
    let mut roots = JobRoots::default();
    for job in unfinished {
        roots.snapshot_ids.extend(job.snapshot_ids.iter().cloned());
        roots.references.extend(job.references.iter().cloned());
        let path = job.directory.join("transfers.sqlite");
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(ProviderError::new(ErrorKind::Transient)),
            Ok(_) => roots.objects.extend(TransferJournal::uploaded(&job.directory, &job.job_id)?),
        }
    }
    roots.objects.sort_by_key(|object| locator_key(&object.locator).unwrap_or_default());
    roots.references.sort_by_key(|object| locator_key(&object.receipt.locator).unwrap_or_default());
    Ok(roots)
}

impl ConnectedRepositoryView<'_> {
    async fn read_points(&self, cancel: &Cancellation) -> Result<Vec<control::ListedBackupPoint>> {
        let mut points = Vec::new();
        let mut cursor: Option<String> = None;
        let mut tracker = PageTracker::default();
        let mut ids = BTreeSet::new();
        loop {
            let page = leases::control_request(cancel, control::list_backup_points_page(
                &self.connected.stored.descriptor, &self.connected.root_key,
                self.connected.provider.as_ref(), &self.connected.handle,
                cursor.as_deref(), 100, cancel,
            )).await?;
            let receipts = page.points.iter().map(|point| point.reference.receipt.clone()).collect::<Vec<_>>();
            tracker.accept(&self.connected.handle, &receipts, page.next_cursor.as_deref())?;
            for point in &page.points {
                if !ids.insert(point.document.point_id.clone()) {
                    return Err(ProviderError::new(ErrorKind::Corrupt));
                }
            }
            points.extend(page.points);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(points),
            }
        }
    }
    async fn bundle_sources(
        &self, points: &[control::ListedBackupPoint], cancel: &Cancellation,
    ) -> Result<BTreeMap<String, BundleSource>> {
        let mut sources = BTreeMap::new();
        let mut identities = BTreeMap::new();
        for point in points {
            for bundle in point.document.bundles() {
                cancel.check()?;
                let identity = reachability::object_identity(bundle, &self.connected.handle)?;
                if let Some(previous) = identities.insert(bundle.object_id.clone(), identity.clone()) {
                    if previous != identity { return Err(ProviderError::new(ErrorKind::Corrupt)); }
                    continue;
                }
                let view = control::read_snapshot_document(self.connected, bundle, cancel).await?;
                let source = match view.captured_by_device {
                    Some(writer_id) => BundleSource::Device { writer_id },
                    None => BundleSource::SyncState { commit_id: view.snapshot_id },
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
            let head = leases::control_request(cancel, control::read_head(
                self.connected.provider.as_ref(), &self.connected.handle,
                &self.connected.stored.descriptor, &self.connected.root_key, None, cancel,
            )).await?.map(|observed| observed.document.state);
            let points = self.read_points(cancel).await?;
            let sources = self.bundle_sources(&points, cancel).await?;
            let decided = points.iter().map(|point| {
                let bundles = point.document.bundles().into_iter().map(|bundle| {
                    Ok(RetentionBundle {
                        object_id: bundle.object_id.clone(),
                        source: sources.get(&bundle.object_id).cloned()
                            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?,
                    })
                }).collect::<Result<Vec<_>>>()?;
                Ok(RetentionPoint {
                    point_id: point.document.point_id.clone(), kind: point.document.kind,
                    created_at_ms: point.document.created_at_ms, bundles,
                })
            }).collect::<Result<Vec<_>>>()?;
            let decision = decide_retention(&decided, self.writer_id, self.policy, self.now_ms);
            let removed: BTreeSet<_> = decision.remove.iter().map(String::as_str).collect();
            let mut roots = ObservedRoots { head, ..ObservedRoots::default() };
            for point in points {
                roots.points.insert(point.document.point_id.clone(), point.reference.clone());
                let bundles = point.document.bundles().into_iter().cloned().collect();
                if removed.contains(point.document.point_id.as_str()) {
                    roots.retired_points.push(RetiredPoint { point: point.reference, bundles });
                } else {
                    roots.kept_points.push(point.reference);
                    roots.kept_bundles.extend(bundles);
                }
            }
            Ok(roots)
        })
    }
    fn snapshots<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, Vec<ObjectReceipt>> {
        Box::pin(async move {
            let mut listed = Vec::new();
            let mut cursor: Option<String> = None;
            let mut tracker = PageTracker::default();
            loop {
                let page = leases::control_request(cancel, self.connected.provider.list_objects(
                    &self.connected.handle, Collection::Snapshots, cursor.as_deref(), 100, cancel,
                )).await?;
                tracker.accept(&self.connected.handle, &page.objects, page.next_cursor.as_deref())?;
                listed.extend(page.objects);
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => return Ok(listed),
                }
            }
        })
    }
    fn job_roots(&self) -> Result<JobRoots> { job_roots_of(&self.unfinished) }
    fn known_objects(&self) -> Result<Vec<RemoteObject>> {
        packaging::known_remote_objects(
            self.cache_root, &self.connected.stored.descriptor.repository_id, &self.connected.handle,
        )
    }
}

pub(crate) struct ConnectedDocuments<'a> {
    pub connected: &'a ConnectedRepository,
    pub cancel: &'a Cancellation,
    pub scratch: &'a Path,
}
impl ConnectedDocuments<'_> {
    fn node(&self, view: &control::SnapshotView) -> Result<DocumentNode> {
        let references = reachability::document_references(view).into_iter()
            .map(|stored| RemoteObject::from_stored(stored, &self.connected.handle))
            .collect::<Result<_>>()?;
        Ok(DocumentNode {
            snapshot_id: view.snapshot_id.clone(), parent_snapshot_id: view.parent_snapshot_id.clone(), references,
        })
    }
}
impl DocumentSource for ConnectedDocuments<'_> {
    fn document<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, DocumentNode> {
        Box::pin(async move {
            let view = control::read_snapshot_document(self.connected, object, self.cancel).await?;
            self.node(&view)
        })
    }
    fn listed<'a>(&'a self, receipt: &'a ObjectReceipt) -> ProviderFuture<'a, (RemoteObject, DocumentNode)> {
        Box::pin(async move {
            let (object, view) = control::open_listed_snapshot(self.connected, receipt.clone(), self.cancel).await?;
            Ok((object, self.node(&view)?))
        })
    }
    fn catalog<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Vec<RemoteObject>> {
        Box::pin(async move { control::read_catalog_children(self.connected, object, self.cancel).await })
    }
    fn probe<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Option<ObjectReceipt>> {
        Box::pin(async move {
            object.stored(&self.connected.handle)?;
            if object.repository_id != self.connected.stored.descriptor.repository_id {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            std::fs::create_dir_all(self.scratch)
                .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
            if crate::trust_boundary::is_link_like(
                &std::fs::symlink_metadata(self.scratch)
                    .map_err(|_| ProviderError::new(ErrorKind::Transient))?,
            ) {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let intent = ObjectIntent {
                repository_id: self.connected.handle.repository_id.clone(), job_id: "cleanup-probe".into(),
                object_id: object.object_id.clone(), role: object.role,
                byte_length: object.receipt.byte_length, sha256: object.ciphertext_sha256.clone(),
            };
            let mut receipt = object.receipt.clone();
            receipt.checksum = None;
            super::transfer_job::verify_remote_receipt(
                self.scratch, &intent, self.connected.provider.as_ref(), &self.connected.handle,
                receipt, self.cancel,
            ).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::{
        fake::{self, DeleteFault, FakeLeaseClock, FakeProvider},
        reachability::tests::{object, source, Source},
    };
    use risunest_external_storage_format::format::{Descriptor, Strategy};
    use std::sync::{atomic::{AtomicUsize, Ordering}, Mutex};

    const NOW: u64 = 1000 * 24 * 60 * leases::MINUTE_MS;
    fn available_time(_: Instant) -> Result<bool> { Ok(true) }
    fn unavailable_time(_: Instant) -> Result<bool> { Ok(false) }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }
    struct View {
        roots: Mutex<ObservedRoots>,
        jobs: Mutex<JobRoots>,
        known: Vec<RemoteObject>,
        reads: AtomicUsize,
        change_at: AtomicUsize,
        listing_error: Mutex<Option<ErrorKind>>,
    }
    impl RepositoryView for View {
        fn roots<'a>(&'a self, _: &'a Cancellation) -> ProviderFuture<'a, ObservedRoots> {
            Box::pin(async move {
                let read = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
                let mut roots = self.roots.lock().unwrap().clone();
                if read >= self.change_at.load(Ordering::SeqCst) {
                    let changed = object("new-point", ObjectRole::BackupPoint);
                    roots.points.insert("new-point".into(), changed);
                }
                Ok(roots)
            })
        }
        fn snapshots<'a>(&'a self, _: &'a Cancellation) -> ProviderFuture<'a, Vec<ObjectReceipt>> {
            Box::pin(async move {
                if let Some(kind) = self.listing_error.lock().unwrap().take() {
                    return Err(ProviderError::new(kind));
                }
                Ok(self.known.iter().filter(|object| matches!(object.role, ObjectRole::SyncState | ObjectRole::BackupBundle))
                    .map(|object| object.receipt.clone()).collect())
            })
        }
        fn job_roots(&self) -> Result<JobRoots> { Ok(self.jobs.lock().unwrap().clone()) }
        fn known_objects(&self) -> Result<Vec<RemoteObject>> { Ok(self.known.clone()) }
    }
    struct Probes<'a> {
        source: &'a Source,
        provider: &'a FakeProvider,
        clock: &'a FakeLeaseClock,
        advance_once: AtomicUsize,
    }
    impl DocumentSource for Probes<'_> {
        fn document<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, DocumentNode> {
            self.source.document(object)
        }
        fn listed<'a>(&'a self, receipt: &'a ObjectReceipt) -> ProviderFuture<'a, (RemoteObject, DocumentNode)> {
            self.source.listed(receipt)
        }
        fn catalog<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Vec<RemoteObject>> {
            self.source.catalog(object)
        }
        fn probe<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Option<ObjectReceipt>> {
            Box::pin(async move {
                self.clock.advance(self.advance_once.swap(0, Ordering::SeqCst) as u64);
                let receipt = self.source.probe(object).await?;
                Ok(if self.provider.holds(&object.object_id) { receipt } else { None })
            })
        }
    }
    struct Harness {
        directory: tempfile::TempDir,
        provider: FakeProvider,
        repository: RepositoryHandle,
        descriptor: Descriptor,
        clock: FakeLeaseClock,
        source: Source,
        view: View,
    }
    impl Harness {
        fn new() -> Self {
            let parent = object("parent-catalog", ObjectRole::Catalog);
            let child = object("child-pack", ObjectRole::Pack);
            let known = vec![parent.clone(), child.clone()];
            let provider = FakeProvider::new(true);
            for object in &known {
                provider.seed(&object.object_id, object.role, vec![1; object.receipt.byte_length as usize]);
            }
            Self {
                directory: tempfile::tempdir().unwrap(), provider, repository: fake::repository(),
                descriptor: Descriptor::new("format-repository".into(), Some(Strategy::Cas)).unwrap(),
                clock: FakeLeaseClock::new(NOW),
                source: source(&known, &[(&parent, vec![child])]),
                view: View {
                    roots: Mutex::new(ObservedRoots::default()), jobs: Mutex::new(JobRoots::default()), known,
                    reads: AtomicUsize::new(0), change_at: AtomicUsize::new(usize::MAX),
                    listing_error: Mutex::new(None),
                },
            }
        }
        fn context(&self) -> LeaseContext<'_> {
            LeaseContext {
                root: self.directory.path(), connection_id: "connection", writer_id: "writer",
                descriptor: &self.descriptor, root_key: &[7; 32], provider: &self.provider,
                repository: &self.repository, clock: &self.clock, protection_supported: true,
            }
        }
        fn probes(&self, advance: u64) -> Probes<'_> {
            Probes { source: &self.source, provider: &self.provider, clock: &self.clock, advance_once: AtomicUsize::new(advance as usize) }
        }
        async fn run(&self) -> Result<CleanupOutcome> {
            run(&self.context(), &CleanupRequest {
                job_id: "cleanup", cleanup_supported: true, connection_time: &available_time,
                limits: CleanupLimits { batch: 1, per_run: 200 },
            }, &self.view, &self.probes(0), &Cancellation::default()).await
        }
        async fn age(&self) {
            assert_eq!(self.run().await.unwrap().deleted_objects, 0);
            self.clock.advance(leases::UNREACHABLE_GRACE_MS);
        }
        fn payload_deletes(&self) -> Vec<String> {
            self.provider.deletion_order().into_iter().filter(|id| id == "parent-catalog" || id == "child-pack").collect()
        }
    }

    #[test]
    fn c_gc_waits_seven_days_and_deletes_parents_before_children() {
        runtime().block_on(async {
            let h = Harness::new();
            h.age().await;
            let result = h.run().await.unwrap();
            assert_eq!(result.stop_reason, StopReason::Complete);
            assert_eq!(result.deleted_objects, 2);
            assert_eq!(h.payload_deletes(), ["parent-catalog", "child-pack"]);
        });
    }
    #[test]
    fn c_unsupported_sends_no_requests_and_untrusted_time_sends_no_delete() {
        runtime().block_on(async {
            let h = Harness::new();
            h.clock.set_trusted(false);
            assert_eq!(h.run().await.unwrap().stop_reason, StopReason::Clock);
            assert!(h.payload_deletes().is_empty());
            let h = Harness::new();
            let result = run(&h.context(), &CleanupRequest {
                job_id: "cleanup", cleanup_supported: false, connection_time: &available_time,
                limits: CleanupLimits::default(),
            }, &h.view, &h.probes(0), &Cancellation::default()).await.unwrap();
            assert_eq!(result.stop_reason, StopReason::Unsupported);
            assert_eq!(h.provider.listing_count(), 0);
            assert!(h.provider.deletion_order().is_empty());
        });
    }
    #[test]
    fn c_connection_time_cannot_be_borrowed_and_a_later_unusable_sample_blocks_delete() {
        runtime().block_on(async {
            // The process clock represents another account's recent usable
            // response. This connection still has no qualifying observation.
            let h = Harness::new();
            let result = run(&h.context(), &CleanupRequest {
                job_id: "cleanup", cleanup_supported: true,
                connection_time: &unavailable_time, limits: CleanupLimits::default(),
            }, &h.view, &h.probes(0), &Cancellation::default()).await.unwrap();
            assert_eq!(result.stop_reason, StopReason::Clock);
            assert!(h.payload_deletes().is_empty());

            // A usable observation may age candidates, but an unusable latest
            // observation for that connection cannot authorize the next run.
            let h = Harness::new();
            let usable = AtomicUsize::new(1);
            let evidence = |_: Instant| Ok(usable.load(Ordering::SeqCst) == 1);
            let request = CleanupRequest {
                job_id: "cleanup", cleanup_supported: true,
                connection_time: &evidence, limits: CleanupLimits::default(),
            };
            assert_eq!(run(
                &h.context(), &request, &h.view, &h.probes(0), &Cancellation::default(),
            ).await.unwrap().deleted_objects, 0);
            h.clock.advance(leases::UNREACHABLE_GRACE_MS);
            usable.store(0, Ordering::SeqCst);
            assert_eq!(run(
                &h.context(), &request, &h.view, &h.probes(0), &Cancellation::default(),
            ).await.unwrap().stop_reason, StopReason::Clock);
            assert!(h.payload_deletes().is_empty());

            // If a later request replaces the usable sample while the survey
            // runs, the next safety checkpoint discards the delete decision.
            let h = Harness::new();
            h.age().await;
            let checks = AtomicUsize::new(0);
            let evidence = |_: Instant| Ok(checks.fetch_add(1, Ordering::SeqCst) < 2);
            let result = run(&h.context(), &CleanupRequest {
                job_id: "cleanup", cleanup_supported: true,
                connection_time: &evidence, limits: CleanupLimits::default(),
            }, &h.view, &h.probes(0), &Cancellation::default()).await.unwrap();
            assert_eq!(result.stop_reason, StopReason::Clock);
            assert!(h.payload_deletes().is_empty());
        });
    }
    #[test]
    fn c_root_replacement_after_mark_prevents_deletion_and_resets_age() {
        runtime().block_on(async {
            let h = Harness::new();
            h.age().await;
            h.view.change_at.store(h.view.reads.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
            assert_eq!(h.run().await.unwrap().stop_reason, StopReason::RootsChanged);
            assert!(h.payload_deletes().is_empty());
            h.view.change_at.store(usize::MAX, Ordering::SeqCst);
            assert_eq!(h.run().await.unwrap().deleted_objects, 0);
        });
    }
    #[test]
    fn c_45_minute_boundary_after_survey_prevents_any_payload_delete() {
        runtime().block_on(async {
            let h = Harness::new();
            h.age().await;
            let result = run(&h.context(), &CleanupRequest {
                job_id: "cleanup", cleanup_supported: true, connection_time: &available_time,
                limits: CleanupLimits::default(),
            }, &h.view, &h.probes(leases::CLEANUP_RUN_LIMIT_MS), &Cancellation::default()).await.unwrap();
            assert_eq!(result.stop_reason, StopReason::Budget);
            assert!(h.payload_deletes().is_empty());
        });
    }
    #[test]
    fn c_partial_list_or_unknown_protection_never_becomes_an_empty_repository() {
        runtime().block_on(async {
            for kind in [ErrorKind::Corrupt, ErrorKind::Transient, ErrorKind::Unauthorized] {
                let h = Harness::new();
                h.age().await;
                *h.view.listing_error.lock().unwrap() = Some(kind);
                assert_eq!(h.run().await.unwrap_err().kind, kind);
                assert!(h.payload_deletes().is_empty());
            }
            let h = Harness::new();
            h.age().await;
            h.provider.seed("unknown-protection", ObjectRole::Lease, b"broken".to_vec());
            assert_eq!(h.run().await.unwrap().stop_reason, StopReason::Lease);
            assert!(h.payload_deletes().is_empty());
        });
    }
    #[test]
    fn c_delete_404_completes_but_202_and_lost_responses_stop_the_batch() {
        runtime().block_on(async {
            for (fault, reason, deleted) in [
                (DeleteFault::NotFound, StopReason::Complete, 2),
                (DeleteFault::Accepted, StopReason::Unsupported, 0),
                (DeleteFault::AppliedThenLost, StopReason::Uncertain, 0),
                (DeleteFault::Transient, StopReason::Uncertain, 0),
            ] {
                let h = Harness::new();
                h.age().await;
                h.provider.fail_delete("parent-catalog", fault);
                let result = h.run().await.unwrap();
                assert_eq!(result.stop_reason, reason);
                assert_eq!(result.deleted_objects, deleted);
                assert_eq!(h.provider.delete_attempts("child-pack"), if deleted == 2 { 1 } else { 0 });
            }
        });
    }
    #[test]
    fn c_401_and_429_keep_their_error_meaning_and_do_not_continue_deleting() {
        runtime().block_on(async {
            for (fault, kind, status) in [
                (DeleteFault::Unauthorized, ErrorKind::Unauthorized, 401),
                (DeleteFault::RateLimited, ErrorKind::RateLimited, 429),
            ] {
                let h = Harness::new();
                h.age().await;
                h.provider.fail_delete("parent-catalog", fault);
                let error = h.run().await.unwrap_err();
                assert_eq!(error.kind, kind);
                assert_eq!(error.http_status, Some(status));
                assert_eq!(h.provider.delete_attempts("child-pack"), 0);
            }
        });
    }
    #[test]
    fn c_lost_delete_is_never_replayed_after_finite_marker_expiry() {
        runtime().block_on(async {
            let h = Harness::new();
            h.age().await;
            h.provider.fail_delete("parent-catalog", DeleteFault::AppliedThenLost);
            assert_eq!(h.run().await.unwrap().stop_reason, StopReason::Uncertain);
            assert_eq!(h.run().await.unwrap().stop_reason, StopReason::Lease);
            h.clock.advance(leases::LEASE_TTL_MS + leases::RELATIVE_CLOCK_ERROR_MS);
            assert_eq!(h.run().await.unwrap().deleted_objects, 0);
            assert_eq!(h.provider.delete_attempts("parent-catalog"), 1);
            assert_eq!(h.provider.delete_attempts("child-pack"), 0);
            h.clock.advance(leases::UNREACHABLE_GRACE_MS);
            assert_eq!(h.run().await.unwrap().deleted_objects, 1);
            assert_eq!(h.provider.delete_attempts("parent-catalog"), 1);
        });
    }
    #[test]
    fn c_batch_limit_discards_the_decision_and_a_new_job_root_keeps_the_child() {
        runtime().block_on(async {
            let h = Harness::new();
            h.age().await;
            let result = run(&h.context(), &CleanupRequest {
                job_id: "cleanup", cleanup_supported: true, connection_time: &available_time,
                limits: CleanupLimits { batch: 1, per_run: 1 },
            }, &h.view, &h.probes(0), &Cancellation::default()).await.unwrap();
            assert_eq!(result.stop_reason, StopReason::Limit);
            assert_eq!(result.deleted_objects, 1);
            h.view.jobs.lock().unwrap().references.push(h.view.known[1].clone());
            assert_eq!(h.run().await.unwrap().deleted_objects, 0);
            assert_eq!(h.provider.delete_attempts("child-pack"), 0);
        });
    }
    #[test]
    fn c_corrupt_job_journal_is_not_silently_dropped_from_protected_roots() {
        let directory = tempfile::tempdir().unwrap();
        let jobs = vec![UnfinishedJob {
            job_id: "job".into(), directory: directory.path().into(),
            snapshot_ids: BTreeSet::new(), references: Vec::new(),
        }];
        assert!(job_roots_of(&jobs).unwrap().objects.is_empty());
        std::fs::write(directory.path().join("transfers.sqlite"), b"broken journal").unwrap();
        assert!(job_roots_of(&jobs).is_err());
    }
    #[test]
    fn c_root_identity_compares_bytes_and_locators_not_only_identifiers() {
        let original = object("head", ObjectRole::SyncState);
        let before = ObservedRoots { head: Some(original.clone()), ..ObservedRoots::default() };
        let mut changed = original;
        changed.ciphertext_sha256 = "ab".repeat(32);
        let after = ObservedRoots { head: Some(changed), ..ObservedRoots::default() };
        assert!(!after.unchanged_from(&before, &fake::repository()).unwrap());
        assert!(before.unchanged_from(&before, &fake::repository()).unwrap());
    }
}
