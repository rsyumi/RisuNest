//! Bounded removal from a fresh, authenticated reachability observation.
//! Every execution owns finite protection and discards its remaining decision
//! when roots, clocks, permissions or remote request outcomes become uncertain.
use super::{
    connection::{decide_retention, RetentionBundle, RetentionPoint, RetentionPolicy},
    connection_commands::ConnectedRepository,
    contract::{
        Cancellation, Collection, ErrorKind, LeaseKind, ObjectIntent, ObjectReceipt, ObjectRole,
        Provider, ProviderError, ProviderFuture, RepositoryHandle, Result,
    },
    control,
    gc_store::{locator_key, GcStore},
    journal::TransferJournal,
    leases::{self, Admission, ClockReading, LeaseContext, LeaseOwner, PageTracker},
    package_cache,
    packaging::{self, RemoteObject},
    reachability::{self, DocumentNode, DocumentSource, MarkRequest, RetiredPoint, Roots},
};
use risunest_external_storage_format::control::BundleSource;
use std::{collections::{BTreeMap, BTreeSet}, path::Path, time::{Duration, Instant}};

// A locator-free lookup can span many individually bounded provider requests.
const INVENTORY_METADATA_TIMEOUT: Duration = Duration::from_secs(3 * 60);

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
        if matches!(object.role,ObjectRole::BackupPoint|ObjectRole::Snapshot|ObjectRole::Segment|ObjectRole::SyncState) {
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

#[derive(Clone, Debug, Default)]
pub(crate) struct InventorySurvey {
    pub pages: Vec<InventoryPageState>,
    pub objects: Vec<RemoteObject>,
}

#[derive(Clone, Debug)]
pub(crate) struct InventoryPageState {
    pub reference: RemoteObject,
    pub operation_id: String,
    pub all_objects_absent: bool,
}

pub(crate) trait RepositoryView: Sync {
    fn roots<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, ObservedRoots>;
    fn snapshots<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, Vec<ObjectReceipt>>;
    fn job_roots(&self) -> Result<JobRoots>;
    fn known_objects(&self) -> Result<Vec<RemoteObject>>;
    fn inventory<'a>(&'a self, _cancel: &'a Cancellation) -> ProviderFuture<'a, InventorySurvey> {
        Box::pin(async { Ok(InventorySurvey::default()) })
    }
    /// The current state of one surveyed page, or `None` once it is gone.
    fn inventory_page<'a>(
        &'a self,
        page: &'a RemoteObject,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Option<InventoryPageState>> {
        Box::pin(async move {
            let key = locator_key(&page.receipt.locator)?;
            Ok(self.inventory(cancel).await?.pages.into_iter().find(|current| {
                locator_key(&current.reference.receipt.locator).ok().as_deref() == Some(key.as_str())
            }))
        })
    }
    fn delete_inventory_page<'a>(
        &'a self,
        _expected: &'a risunest_external_storage_format::snapshot::StoredObject,
        _cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, control::RemoteInventoryPageDeleteOutcome> {
        Box::pin(async { Err(ProviderError::new(ErrorKind::Unsupported)) })
    }
    fn confirmed_removed(&self, _object: &RemoteObject) -> Result<()> { Ok(()) }
    fn prepare_removals(&self, _objects: &[RemoteObject]) -> Result<()> { Ok(()) }
    fn protected_jobs(&self) -> Vec<String> { Vec::new() }
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

fn trusted_wall(context: &LeaseContext<'_>, started: ClockReading) -> Option<u64> {
    let now = context.clock.reading();
    (now.trusted && now.foreground && now.epoch == started.epoch).then_some(now.wall_ms)
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
    let bookkeeping = record(context, &outcome);
    // A statistics failure must not turn 401, 429 or cancellation into a generic
    // storage error.
    result?;
    bookkeeping?;
    Ok(outcome)
}

/// Without compaction an idle repository only gains unreachable inputs as they
/// age, so cleanup revisits it once a day.
pub(crate) const LWW_CLEANUP_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;

pub(crate) fn lww_cleanup_due(compacted: bool, last_run_ms: Option<u64>, now_ms: u64) -> bool {
    compacted || last_run_ms.is_none_or(|last| {
        now_ms.checked_sub(last).is_none_or(|elapsed| elapsed >= LWW_CLEANUP_INTERVAL_MS)
    })
}

/// Cleanup of a serverless repository. `store` holds the segment references
/// this device recorded and the versions it releases with a removed segment.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_lww(
    context:&LeaseContext<'_>,request:&CleanupRequest<'_>,engine:&super::lww_engine::ExternalLwwEngine,
    store:&std::sync::Mutex<crate::persistent_store::PersistentStore>,
    base:ConnectedRepositoryView<'_>,scratch:&Path,cancel:&Cancellation,
)->Result<CleanupOutcome> {
    if engine.connection_id!=context.connection_id || engine.repository.repository_id!=context.repository.repository_id || engine.repository.connection_identity!=context.repository.connection_identity
        || engine.library!=context.descriptor.repository_id {return Err(ProviderError::new(ErrorKind::Corrupt))}
    let documents=ConnectedDocuments{connected:base.connected,cancel,scratch};
    let view=LwwRepositoryView::new(engine,store,base,documents);
    run(context,request,&view,&view,cancel).await
}

fn finish_delete<T>(owner: &LeaseOwner, result: Result<T>) -> Result<T> {
    if result.as_ref().map_or_else(|error| {
        error.http_status != Some(202)
            && !matches!(error.kind, ErrorKind::Transient | ErrorKind::Cancelled)
    }, |_| true) {
        owner.set_delete_in_flight(false);
    }
    result
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
    let inventory = view.inventory(cancel).await?;
    let jobs = view.job_roots()?;
    let mut known_objects = view.known_objects()?;
    known_objects.extend(inventory.objects);
    // Observations outlive this run, so they are stamped only with trusted time.
    let Some(observed_at) = trusted_wall(context, started) else {
        outcome.stop_reason = StopReason::Clock;
        return Ok(());
    };
    let marked = reachability::mark(context.root, source, MarkRequest {
        connection_id: context.connection_id, repository: context.repository,
        format_repository_id: &context.descriptor.repository_id,
        now_ms: observed_at,
        roots: Roots {
            head: before.head.clone(), kept_points: before.kept_points.clone(),
            kept_bundles: before.kept_bundles.clone(), job_objects: jobs.objects.clone(),
            job_references: jobs.references.clone(), job_snapshot_ids: jobs.snapshot_ids.clone(),
        },
        listed, known_objects, retired_points: before.retired_points.clone(),
    }, cancel).await?;
    if let Some(reason) = current_limit(context, request, time_not_before, owner, started, cancel)? {
        outcome.stop_reason = reason;
        return Ok(());
    }
    let mut marker_placed = !marked.candidates.is_empty();
    if marker_placed {
        owner.renew_if_due(context, cancel).await?;
        owner.place_marker(context, cancel).await?;
    }
    if let Some(reason) = recheck(context, owner, view, &before, &jobs, cancel).await? {
        outcome.stop_reason = reason;
        return Ok(());
    }
    outcome.stop_reason = StopReason::Complete;
    let mut sent = 0;
    let mut containers = Vec::new();
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
        // Invalidate the bounded candidate batch before deletion. An interrupted
        // batch may discard reusable hints, but can never leave a deleted hint.
        view.prepare_removals(batch)?;
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
                view.confirmed_removed(object)?;
                containers.push(object.receipt.locator.clone());
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
                view.confirmed_removed(object)?;
                containers.push(object.receipt.locator.clone());
                before.removed(object);
                outcome.deleted_objects = outcome.deleted_objects.checked_add(1)
                    .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
                outcome.deleted_bytes = outcome.deleted_bytes.checked_add(object.receipt.byte_length)
                    .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
            }
        }
    }
    containers.sort_by(|a, b| a.collection.cmp(&b.collection));
    containers.dedup_by(|a, b| a.collection == b.collection);
    let mut protected_jobs = view.protected_jobs();
    protected_jobs.push(request.job_id.to_owned());
    for locator in containers {
        if !matches!(outcome.stop_reason, StopReason::Complete | StopReason::Limit) { break; }
        if let Some(reason) = current_limit(context, request, time_not_before, owner, started, cancel)? {
            outcome.stop_reason = reason;
            break;
        }
        owner.renew_if_due(context, cancel).await?;
        if let Some(reason) = recheck(context, owner, view, &before, &jobs, cancel).await? {
            outcome.stop_reason = reason;
            break;
        }
        owner.set_delete_in_flight(true);
        let result = leases::control_request(cancel, context.provider.delete_empty_container(
            context.repository, &locator, &protected_jobs, cancel,
        )).await;
        finish_delete(owner, result)?;
    }
    if matches!(outcome.stop_reason, StopReason::Complete | StopReason::Limit) {
        let protected = view.protected_jobs().into_iter().collect::<BTreeSet<_>>();
        let fresh = view.inventory(cancel).await?;
        let mut absent = BTreeMap::new();
        let mut pages = BTreeMap::new();
        for page in fresh.pages {
            let key = locator_key(&page.reference.receipt.locator)?;
            let identity = reachability::object_identity(&page.reference, context.repository)?;
            if page.all_objects_absent && !protected.contains(&page.operation_id) {
                absent.insert(key.clone(), identity.clone());
            }
            if pages.insert(key, (identity, page)).is_some() {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
        }
        let Some(observed_at) = trusted_wall(context, started) else {
            outcome.stop_reason = StopReason::Clock;
            return Ok(());
        };
        let ages = GcStore::open(context.root)?.record_inventory_page_observations(
            context.connection_id,
            &absent,
            observed_at,
        )?;
        for (key, first_absent) in ages {
            if sent >= request.limits.per_run {
                outcome.stop_reason = StopReason::Limit;
                break;
            }
            if observed_at.saturating_sub(first_absent) < leases::UNREACHABLE_GRACE_MS {
                continue;
            }
            if !marker_placed {
                owner.renew_if_due(context, cancel).await?;
                owner.place_marker(context, cancel).await?;
                marker_placed = true;
            }
            let Some((expected_identity, expected)) = pages.get(&key) else {
                continue;
            };
            if let Some(reason) = current_limit(
                context,
                request,
                time_not_before,
                owner,
                started,
                cancel,
            )? {
                outcome.stop_reason = reason;
                break;
            }
            if let Some(reason) = recheck(context, owner, view, &before, &jobs, cancel).await? {
                outcome.stop_reason = reason;
                break;
            }
            let Some(current) = view.inventory_page(&expected.reference, cancel).await? else {
                GcStore::open(context.root)?
                    .forget_inventory_page_observation(context.connection_id, &key)?;
                continue;
            };
            if !current.all_objects_absent
                || protected.contains(&current.operation_id)
                || reachability::object_identity(&current.reference, context.repository)?
                    != *expected_identity
            {
                GcStore::open(context.root)?
                    .forget_inventory_page_observation(context.connection_id, &key)?;
                continue;
            }
            let stored = current.reference.stored(context.repository)?;
            if let Some(reason) = current_limit(
                context, request, time_not_before, owner, started, cancel,
            )? {
                outcome.stop_reason = reason;
                break;
            }
            owner.set_delete_in_flight(true);
            let removed = leases::control_request(
                cancel, view.delete_inventory_page(&stored, cancel),
            ).await;
            match finish_delete(owner, removed)? {
                control::RemoteInventoryPageDeleteOutcome::Deleted
                | control::RemoteInventoryPageDeleteOutcome::NotFound => {
                    sent += 1;
                    GcStore::open(context.root)?
                        .forget_inventory_page_observation(context.connection_id, &key)?;
                    outcome.deleted_objects = outcome.deleted_objects.saturating_add(1);
                    outcome.deleted_bytes = outcome
                        .deleted_bytes
                        .saturating_add(expected.reference.receipt.byte_length);
                }
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
            if sent < request.limits.per_run && context.provider.supports_incomplete_cleanup() {
                if !marker_placed { owner.place_marker(context, cancel).await?; }
                if let Some(reason) = recheck(context, owner, view, &before, &jobs, cancel).await? {
                    outcome.stop_reason = reason;
                    return Ok(());
                }
                let mut protected = view.protected_jobs();
                protected.push(request.job_id.to_owned());
                let mut cursor = None;
                loop {
                    if let Some(reason) = current_limit(context, request, time_not_before, owner, started, cancel)? {
                        outcome.stop_reason = reason;
                        break;
                    }
                    if let Some(reason) = recheck(context, owner, view, &before, &jobs, cancel).await? {
                        outcome.stop_reason = reason;
                        break;
                    }
                    let page = leases::control_request(cancel, context.provider.cleanup_incomplete_objects(
                        context.repository, &protected, cursor.as_deref(), request.limits.per_run - sent,
                        &|deleting| owner.set_delete_in_flight(deleting), cancel,
                    )).await?;
                    sent = sent.saturating_add(page.removed as usize);
                    outcome.deleted_objects = outcome.deleted_objects.saturating_add(page.removed);
                    cursor = page.next_cursor;
                    if cursor.is_none() { break; }
                    if sent >= request.limits.per_run {
                        outcome.stop_reason = StopReason::Limit;
                        break;
                    }
                }
            }
            GcStore::open(context.root)?.set_last_reachable_bytes(context.connection_id, marked.reachable_bytes)?;
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
    pub reads: RootReads,
}
/// The roots one view has already read. Every recheck lists the points again
/// and downloads only what changed: a point is written once under its own
/// name and a bundle is named by its content, so an equal receipt or identity
/// still names the document read before.
#[derive(Default)]
pub(crate) struct RootReads {
    points: std::sync::Mutex<BTreeMap<String, control::ListedBackupPoint>>,
    bundles: std::sync::Mutex<BTreeMap<String, BundleSource>>,
}
pub(crate) struct UnfinishedJob {
    pub job_id: String,
    pub directory: std::path::PathBuf,
    pub snapshot_ids: BTreeSet<String>,
    /// Includes capture, conflict and export references held outside the journal.
    pub references: Vec<RemoteObject>,
}

fn job_roots_of(unfinished: &[UnfinishedJob], repository: &RepositoryHandle) -> Result<JobRoots> {
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
    // A stopped publication is still entitled to the parent it selected, and
    // its own inventory does not name what it reused without uploading. A
    // stopped check likewise still owns the root it resumes on, which may be
    // a head that has since moved on.
    for job in unfinished {
        for root in TransferJournal::parent(&job.directory)? {
            roots.references.push(super::packaging::RemoteObject::from_stored(&root, repository)?);
        }
        roots.references.extend(super::repository_check::pinned_root(&job.directory, repository)?);
    }
    roots.objects.sort_by_key(|object| locator_key(&object.locator).unwrap_or_default());
    roots.references.sort_by_key(|object| locator_key(&object.receipt.locator).unwrap_or_default());
    Ok(roots)
}

impl ConnectedRepositoryView<'_> {
    async fn roots_with_head(&self, cancel:&Cancellation, include_head:bool)->Result<ObservedRoots> {
            let head = if include_head && self.connected.stored.descriptor.publication_strategy.is_some() {
                leases::control_request(cancel, control::read_head(
                    self.connected.provider.as_ref(), &self.connected.handle,
                    &self.connected.stored.descriptor, &self.connected.root_key, None, cancel,
                )).await?.map(|observed| observed.document.state)
            } else {
                // Backup-only repositories have authenticated points, not a
                // mutable synchronization head. Do not probe a fictitious one.
                None
            };
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
    }
    async fn read_points(&self, cancel: &Cancellation) -> Result<Vec<control::ListedBackupPoint>> {
        let connected = self.connected;
        let mut earlier = std::mem::take(&mut *self.reads.points.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        let mut current = BTreeMap::new();
        let mut points = Vec::new();
        let mut cursor: Option<String> = None;
        let mut tracker = PageTracker::default();
        let mut ids = BTreeSet::new();
        loop {
            let next = leases::control_request(cancel, async {
                let page = connected.provider.list_objects(
                    &connected.handle, Collection::BackupPoints, cursor.as_deref(), 100, cancel,
                ).await?;
                tracker.accept(&connected.handle, &page.objects, page.next_cursor.as_deref())?;
                for receipt in page.objects {
                    cancel.check()?;
                    let key = locator_key(&receipt.locator)?;
                    let point = match earlier.remove(&key).filter(|point| point.reference.receipt == receipt) {
                        Some(point) => point,
                        None => control::open_listed_point(
                            receipt, &connected.stored.descriptor, &connected.root_key,
                            connected.provider.as_ref(), &connected.handle, cancel,
                        ).await?,
                    };
                    if !ids.insert(point.document.point_id.clone()) {
                        return Err(ProviderError::new(ErrorKind::Corrupt));
                    }
                    current.insert(key, point.clone());
                    points.push(point);
                }
                Ok(page.next_cursor)
            }).await?;
            match next {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        *self.reads.points.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = current;
        Ok(points)
    }
    async fn bundle_sources(
        &self, points: &[control::ListedBackupPoint], cancel: &Cancellation,
    ) -> Result<BTreeMap<String, BundleSource>> {
        let mut earlier = std::mem::take(&mut *self.reads.bundles.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        let mut current = BTreeMap::new();
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
                let source = match earlier.remove(&identity) {
                    Some(source) => source,
                    None => {
                        let view = control::read_snapshot_document(self.connected, bundle, cancel).await?;
                        match view.captured_by_device {
                            Some(writer_id) => BundleSource::Device { writer_id },
                            None => BundleSource::SyncState { commit_id: view.snapshot_id },
                        }
                    }
                };
                current.insert(identity, source.clone());
                sources.insert(bundle.object_id.clone(), source);
            }
        }
        *self.reads.bundles.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = current;
        Ok(sources)
    }

    async fn read_inventory(&self, cancel: &Cancellation) -> Result<InventorySurvey> {
        self.read_inventory_in_scope(cancel,None).await
    }
    async fn read_inventory_in_scope(&self,cancel:&Cancellation,extra_repository_id:Option<&str>)->Result<InventorySurvey> {
        let mut survey = InventorySurvey::default();
        let mut cursor: Option<String> = None;
        let mut tracker = PageTracker::default();
        let mut page_ids = BTreeSet::new();
        loop {
            let page = leases::control_request(
                cancel,
                control::list_inventory_pages_page_in_scope(
                    &self.connected.stored.descriptor,
                    &self.connected.root_key,
                    self.connected.provider.as_ref(),
                    &self.connected.handle,
                    cursor.as_deref(),
                    100,
                    cancel,
                    extra_repository_id,
                ),
            )
            .await?;
            let receipts = page
                .pages
                .iter()
                .map(|page| page.reference.receipt.clone())
                .collect::<Vec<_>>();
            tracker.accept(
                &self.connected.handle,
                &receipts,
                page.next_cursor.as_deref(),
            )?;
            for listed in page.pages {
                if !page_ids.insert(listed.document.page_id.clone()) {
                    return Err(ProviderError::new(ErrorKind::Corrupt));
                }
                let state = self.page_state(listed, &mut survey.objects, cancel).await?;
                survey.pages.push(state);
            }
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => return Ok(survey),
            }
        }
    }
    /// Reads one page again by the receipt a survey listed it under.
    async fn inventory_page_in_scope(
        &self, page: &RemoteObject, cancel: &Cancellation, extra_repository_id: Option<&str>,
    ) -> Result<Option<InventoryPageState>> {
        let listed = match leases::control_request(cancel, control::open_listed_inventory_page_in_scope(
            page.receipt.clone(), &self.connected.stored.descriptor, &self.connected.root_key,
            self.connected.provider.as_ref(), &self.connected.handle, cancel, extra_repository_id,
        )).await {
            Err(error) if error.kind == ErrorKind::NotFound => return Ok(None),
            other => other?,
        };
        Ok(Some(self.page_state(listed, &mut Vec::new(), cancel).await?))
    }
    async fn page_state(
        &self, listed: control::ListedInventoryPage, objects: &mut Vec<RemoteObject>, cancel: &Cancellation,
    ) -> Result<InventoryPageState> {
        let mut present = 0usize;
        for entry in &listed.document.objects {
            let role = if entry.role == risunest_external_storage_format::snapshot::ObjectRole::SyncState
                && uuid::Uuid::parse_str(&entry.object_id).is_ok_and(|id| id.to_string()==entry.object_id)
            {
                ObjectRole::Snapshot
            } else {
                packaging::native_role(entry.role)?
            };
            let intent = ObjectIntent {
                repository_id: self.connected.handle.repository_id.clone(),
                job_id: listed.document.operation_id.clone(),
                object_id: entry.object_id.clone(),
                role,
                byte_length: entry.ciphertext_length,
                sha256: hex::encode(entry.ciphertext_sha256),
            };
            let Some(receipt) = leases::control_request_with_timeout(
                cancel,
                self.connected.provider.lookup_metadata(
                    &self.connected.handle,
                    &intent,
                    None,
                    cancel,
                ),
                INVENTORY_METADATA_TIMEOUT,
            )
            .await?
            else {
                continue;
            };
            super::journal::validate_receipt(
                &intent,
                &self.connected.handle,
                &receipt,
            )?;
            present += 1;
            objects.push(RemoteObject {
                repository_id: listed.document.repository_id.clone(),
                object_id: entry.object_id.clone(),
                role,
                receipt,
                ciphertext_sha256: intent.sha256,
                plaintext_length: entry.plaintext_length,
                plaintext_sha256: hex::encode(entry.plaintext_sha256),
            });
        }
        Ok(InventoryPageState {
            reference: listed.reference,
            operation_id: listed.document.operation_id,
            all_objects_absent: present == 0,
        })
    }
}
impl RepositoryView for ConnectedRepositoryView<'_> {
    fn roots<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, ObservedRoots> {
        Box::pin(self.roots_with_head(cancel,true))
    }
    fn snapshots<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, Vec<ObjectReceipt>> {
        Box::pin(async move {
            // Checkpoints are listed apart from the snapshots they are stored
            // beside, and a cleanup still has to see every one of them.
            let mut listed = Vec::new();
            let mut tracker = PageTracker::default();
            for collection in [Collection::Snapshots, Collection::Checkpoints] {
                tracker.next_listing();
                let mut cursor: Option<String> = None;
                loop {
                    let page = leases::control_request(cancel, self.connected.provider.list_objects(
                        &self.connected.handle, collection, cursor.as_deref(), 100, cancel,
                    )).await?;
                    tracker.accept(&self.connected.handle, &page.objects, page.next_cursor.as_deref())?;
                    listed.extend(page.objects);
                    match page.next_cursor {
                        Some(next) => cursor = Some(next),
                        None => break,
                    }
                }
            }
            Ok(listed)
        })
    }
    fn job_roots(&self) -> Result<JobRoots> { job_roots_of(&self.unfinished, &self.connected.handle) }
    fn prepare_removals(&self, objects: &[RemoteObject]) -> Result<()> {
        let cache = package_cache::PackageCache::open(self.cache_root)?;
        let ids: Vec<&str> = objects.iter().map(|object| object.object_id.as_str()).collect();
        cache.forget_objects(&self.connected.stored.descriptor.repository_id, &self.connected.handle, &ids)
    }
    fn protected_jobs(&self) -> Vec<String> {
        self.unfinished.iter().map(|job| job.job_id.clone()).collect()
    }
    fn known_objects(&self) -> Result<Vec<RemoteObject>> {
        package_cache::known_remote_objects(
            self.cache_root, &self.connected.stored.descriptor.repository_id, &self.connected.handle,
        )
    }
    fn inventory<'a>(&'a self, cancel: &'a Cancellation) -> ProviderFuture<'a, InventorySurvey> {
        Box::pin(self.read_inventory(cancel))
    }
    fn inventory_page<'a>(
        &'a self,
        page: &'a RemoteObject,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Option<InventoryPageState>> {
        Box::pin(self.inventory_page_in_scope(page, cancel, None))
    }
    fn delete_inventory_page<'a>(
        &'a self,
        expected: &'a risunest_external_storage_format::snapshot::StoredObject,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, control::RemoteInventoryPageDeleteOutcome> {
        Box::pin(control::delete_authenticated_inventory_page(
            self.connected,
            expected,
            cancel,
        ))
    }
}

pub(crate) struct ConnectedDocuments<'a> {
    pub connected: &'a ConnectedRepository,
    pub cancel: &'a Cancellation,
    pub scratch: &'a Path,
}

type ListedCheckpoint=Option<(RemoteObject,super::lww_checkpoint::Checkpoint)>;
/// One cleanup run reads each published checkpoint once and each segment at
/// most once; a segment whose references this device recorded is not read.
/// Rechecks only list again. A changed receipt is read again.
pub(crate) struct LwwRepositoryView<'a> {
    pub engine:&'a super::lww_engine::ExternalLwwEngine,
    lww_repository_id:std::borrow::Cow<'a,str>,
    pub base:ConnectedRepositoryView<'a>,
    pub documents:ConnectedDocuments<'a>,
    store:&'a std::sync::Mutex<crate::persistent_store::PersistentStore>,
    native_bodies:std::sync::Mutex<BTreeMap<String,RemoteObject>>,
    known:std::sync::Mutex<Vec<RemoteObject>>,
    checkpoints:std::sync::Mutex<BTreeMap<String,ListedCheckpoint>>,
    segments:std::sync::Mutex<BTreeMap<String,(RemoteObject,DocumentNode)>>,
    coverage:std::sync::Mutex<super::lww_checkpoint::Coverage>,
}
fn receipt_key(receipt:&ObjectReceipt)->Result<String> {
    serde_json::to_string(receipt).map_err(|_|ProviderError::new(ErrorKind::Corrupt))
}
impl<'a> LwwRepositoryView<'a> {
    pub(crate) fn new(engine:&'a super::lww_engine::ExternalLwwEngine,store:&'a std::sync::Mutex<crate::persistent_store::PersistentStore>,
        base:ConnectedRepositoryView<'a>,documents:ConnectedDocuments<'a>)->Self {
        Self{engine,lww_repository_id:super::lww_segment::format_repository_id(&engine.repository.repository_id),
            base,documents,store,native_bodies:Default::default(),known:Default::default(),checkpoints:Default::default(),
            segments:Default::default(),coverage:Default::default()}
    }
    fn store(&self)->Result<std::sync::MutexGuard<'_,crate::persistent_store::PersistentStore>> {
        self.store.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))
    }
    async fn checkpoint_document(&self,receipt:&ObjectReceipt)->Result<ListedCheckpoint> {
        let key=receipt_key(receipt)?;
        if let Some(found)=self.checkpoints.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?.get(&key) {return Ok(found.clone())}
        let found=self.engine.classified_checkpoint(receipt,self.documents.cancel).await?;
        self.checkpoints.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?.insert(key,found.clone());
        Ok(found)
    }
    /// A published input this run already downloaded and authenticated.
    fn verified(&self,object:&RemoteObject)->Result<Option<ObjectReceipt>> {
        let key=receipt_key(&object.receipt)?;
        Ok(match object.role {
            ObjectRole::Segment=>self.segments.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?
                .get(&key).map(|(current,_)|current.receipt.clone()),
            ObjectRole::Snapshot|ObjectRole::SyncState=>self.checkpoints.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?
                .get(&key).and_then(Option::as_ref).map(|(current,_)|current.receipt.clone()),
            _=>None,
        })
    }
    async fn segment_document(&self,receipt:&ObjectReceipt)->Result<(RemoteObject,DocumentNode)> {
        let key=receipt_key(receipt)?;
        if let Some(found)=self.segments.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?.get(&key) {return Ok(found.clone())}
        let found=self.segment_node(receipt).await?;
        self.segments.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?.insert(key,found.clone());
        Ok(found)
    }
    fn standalone(&self,hash:&str,body:&super::lww_segment::LargeBody)->Result<RemoteObject> {
        let locator=body.locator.clone().ok_or_else(||ProviderError::new(ErrorKind::Corrupt))?;
        locator.validate_for(&self.engine.repository)?;
        risunest_sync_wire::validate_hash(hash).map_err(|_|ProviderError::new(ErrorKind::Corrupt))?;
        let object=RemoteObject{repository_id:self.engine.repository.repository_id.clone(),object_id:body.object_id.clone(),
            role:ObjectRole::Pack,receipt:ObjectReceipt{locator,byte_length:body.byte_length.0,version:None,checksum:None,complete:true},
            ciphertext_sha256:body.sha256.clone(),plaintext_length:body.plaintext_byte_length.0,plaintext_sha256:hash.into()};
        let key=locator_key(&object.receipt.locator)?;
        let mut bodies=self.native_bodies.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?;
        if let Some(previous)=bodies.insert(key,object.clone()) {
            if reachability::native_identity(&previous,&self.engine.repository)?!=reachability::native_identity(&object,&self.engine.repository)? {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
        }
        Ok(object)
    }
    fn checkpoint_node(&self,checkpoint:&super::lww_checkpoint::Checkpoint)->Result<DocumentNode> {
        let references=std::iter::once(&checkpoint.library.record_catalog).chain(std::iter::once(&checkpoint.library.asset_catalog))
            .chain(checkpoint.asset_catalogs.iter()).map(|object|RemoteObject::from_stored(object,&self.engine.repository))
            .collect::<Result<Vec<_>>>()?;
        let mut node=DocumentNode{snapshot_id:checkpoint.snapshot_id.clone(),parent_snapshot_id:None,references};
        for (hash,body) in &checkpoint.standalone_bodies {node.references.push(self.standalone(hash,body)?);}
        Ok(node)
    }
    async fn segment_node(&self,receipt:&ObjectReceipt)->Result<(RemoteObject,DocumentNode)> {
        let name=receipt.locator.object.rsplit('/').next().ok_or_else(||ProviderError::new(ErrorKind::Corrupt))?;
        let (writer,seq,hash)=super::contract::parse_segment_object_id(name)?;
        let target=self.engine.target_scope();
        let cached=self.store()?.external_lww_segment_references(&target,writer,seq,hash).map_err(super::lww_engine::store_error)?;
        if let Some(references)=cached.filter(|references|receipt.complete && references.byte_length==receipt.byte_length) {
            return self.referenced_node(receipt,name,hash,&references);
        }
        let bytes=super::lww_engine::read_bytes(self.engine.provider.as_ref(),&self.engine.repository,&receipt.locator,self.documents.cancel).await?;
        if !receipt.complete || bytes.len() as u64!=receipt.byte_length || super::lww_segment::digest(&bytes)!=hash {return Err(ProviderError::new(ErrorKind::Corrupt))}
        let document=super::lww_segment::open(&bytes,&self.engine.library,writer,seq,&self.engine.root_key)?;
        let references=super::lww_engine::segment_references(receipt.byte_length,&document)?;
        let node=self.referenced_node(receipt,name,hash,&references)?;
        self.store()?.external_lww_record_seen(&target,writer,seq,hash,Some(&references)).map_err(super::lww_engine::store_error)?;
        Ok(node)
    }
    fn referenced_node(&self,receipt:&ObjectReceipt,name:&str,hash:&str,document:&crate::persistent_store::external_lww::SegmentReferences)->Result<(RemoteObject,DocumentNode)> {
        let upper=self.engine.admitted_upper()?.checked_add(300_000).ok_or_else(||ProviderError::new(ErrorKind::Corrupt))?;
        if document.newest_physical_ms>upper {return Err(super::clock::future_change("cleanup",document.newest_physical_ms,upper))}
        let object=RemoteObject{repository_id:self.engine.repository.repository_id.clone(),object_id:name.into(),role:ObjectRole::Segment,
            receipt:receipt.clone(),ciphertext_sha256:hash.into(),plaintext_length:document.plaintext_length,
            plaintext_sha256:document.plaintext_sha256.clone()};
        let mut references=document.asset_catalogs.iter().chain(&document.data_catalogs).map(|object|RemoteObject::from_stored(object,&self.engine.repository)).collect::<Result<Vec<_>>>()?;
        for (hash,body) in &document.large_bodies {references.push(self.standalone(hash,body)?);}
        Ok((object,DocumentNode{snapshot_id:name.into(),parent_snapshot_id:None,references}))
    }
}
impl RepositoryView for LwwRepositoryView<'_> {
    fn roots<'a>(&'a self,cancel:&'a Cancellation)->ProviderFuture<'a,ObservedRoots> {
        Box::pin(async move {
            let mut roots=self.base.roots_with_head(cancel,false).await?;
            std::fs::create_dir_all(self.documents.scratch).map_err(|_|ProviderError::new(ErrorKind::Transient))?;
            let metadata=std::fs::symlink_metadata(self.documents.scratch).map_err(|_|ProviderError::new(ErrorKind::Transient))?;
            if !metadata.is_dir() || crate::trust_boundary::is_link_like(&metadata) {return Err(ProviderError::new(ErrorKind::Corrupt))}
            let mut checkpoints=Vec::new();
            for receipt in self.engine.checkpoint_receipts(cancel).await? {
                if let Some(found)=self.checkpoint_document(&receipt).await? {checkpoints.push(found);}
            }
            let retained=super::lww_checkpoint::retained(&checkpoints.iter().map(|(_,doc)|doc.clone()).collect::<Vec<_>>())?;
            let mut coverage=super::lww_checkpoint::Coverage::new();let mut known=Vec::new();
            for (object,document) in &checkpoints {
                self.checkpoint_node(document)?;
                if roots.points.insert(format!("lww/{}",object.object_id),object.clone()).is_some() {return Err(ProviderError::new(ErrorKind::Corrupt))}
                if retained.contains(&document.snapshot_id) {
                    roots.kept_bundles.push(object.clone());
                    for (writer,prefix) in &document.covered_prefixes {coverage.entry(writer.clone()).and_modify(|old|*old=(*old).max(*prefix)).or_insert(*prefix);}
                }
                known.push(object.clone());
            }
            let mut identities=BTreeMap::new();
            for receipt in self.engine.listing(cancel).await? {
                let name=receipt.locator.object.rsplit('/').next().ok_or_else(||ProviderError::new(ErrorKind::Corrupt))?;
                let (writer,seq,hash)=super::contract::parse_segment_object_id(name)?;
                if identities.insert((writer.to_owned(),seq),hash.to_owned()).is_some_and(|previous|previous!=hash) {
                    return Err(ProviderError::new(ErrorKind::Corrupt));
                }
                let (object,_)=self.segment_document(&receipt).await?;
                roots.points.insert(format!("lww/{}",object.object_id),object.clone());
                if seq>coverage.get(writer).map_or(0,|prefix|prefix.0) {roots.kept_bundles.push(object.clone());}
                known.push(object);
            }
            known.extend(self.native_bodies.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?.values().cloned());
            *self.known.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?=known;
            *self.coverage.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?=coverage;
            Ok(roots)
        })
    }
    fn snapshots<'a>(&'a self,cancel:&'a Cancellation)->ProviderFuture<'a,Vec<ObjectReceipt>> {
        Box::pin(async move {let mut listed=self.base.snapshots(cancel).await?;listed.extend(self.engine.listing(cancel).await?);Ok(listed)})
    }
    fn job_roots(&self)->Result<JobRoots> {self.base.job_roots()}
    fn known_objects(&self)->Result<Vec<RemoteObject>> {
        let mut known=self.base.known_objects()?;known.extend(self.known.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?.clone());Ok(known)
    }
    fn inventory<'a>(&'a self,cancel:&'a Cancellation)->ProviderFuture<'a,InventorySurvey> {Box::pin(self.base.read_inventory_in_scope(cancel,Some(&self.lww_repository_id)))}
    fn inventory_page<'a>(&'a self,page:&'a RemoteObject,cancel:&'a Cancellation)->ProviderFuture<'a,Option<InventoryPageState>> {
        Box::pin(self.base.inventory_page_in_scope(page,cancel,Some(&self.lww_repository_id)))
    }
    fn delete_inventory_page<'a>(&'a self,expected:&'a risunest_external_storage_format::snapshot::StoredObject,cancel:&'a Cancellation)->ProviderFuture<'a,control::RemoteInventoryPageDeleteOutcome> {
        if expected.header.repository_id==self.lww_repository_id {
            Box::pin(control::delete_authenticated_inventory_page_for_repository(self.base.connected,expected,&self.lww_repository_id,cancel))
        } else {self.base.delete_inventory_page(expected,cancel)}
    }
    fn prepare_removals(&self,objects:&[RemoteObject])->Result<()> {self.base.prepare_removals(objects)}
    fn confirmed_removed(&self,object:&RemoteObject)->Result<()> {
        self.base.confirmed_removed(object)?;
        if object.role!=ObjectRole::Segment {return Ok(())}
        let (writer,seq,hash)=super::contract::parse_segment_object_id(&object.object_id)?;
        let covered=self.coverage.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?.get(writer).is_some_and(|prefix|seq<=prefix.0);
        if covered {
            self.store()?.external_lww_forget_removed_segment(&self.engine.target_scope(),writer,seq,hash).map_err(super::lww_engine::store_error)?;
        }
        Ok(())
    }
    fn protected_jobs(&self)->Vec<String> {self.base.protected_jobs()}
}
impl DocumentSource for LwwRepositoryView<'_> {
    fn format_repository_id(&self,object:&RemoteObject)->Option<&str> {
        if object.repository_id==self.engine.repository.repository_id {Some(&self.engine.repository.repository_id)}
        else {(object.repository_id==self.lww_repository_id).then_some(self.lww_repository_id.as_ref())}
    }
    fn native_body(&self,object:&RemoteObject)->Result<bool> {
        if object.role!=ObjectRole::Pack {return Ok(false)}
        let bodies=self.native_bodies.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?;
        let Some(proof)=bodies.get(&locator_key(&object.receipt.locator)?) else {return Ok(false)};
        if reachability::native_identity(proof,&self.engine.repository)?!=reachability::native_identity(object,&self.engine.repository)? {return Err(ProviderError::new(ErrorKind::Corrupt))}
        Ok(true)
    }
    fn document<'a>(&'a self,object:&'a RemoteObject)->ProviderFuture<'a,DocumentNode> {
        Box::pin(async move {
            match object.role {
                ObjectRole::Snapshot|ObjectRole::SyncState if !object.object_id.starts_with("snapshot-")=>{let (current,document)=self.checkpoint_document(&object.receipt).await?.ok_or_else(||ProviderError::new(ErrorKind::Corrupt))?;
                    if reachability::object_identity(&current,&self.engine.repository)?!=reachability::object_identity(object,&self.engine.repository)? {return Err(ProviderError::new(ErrorKind::Corrupt))}self.checkpoint_node(&document)},
                ObjectRole::Segment=>{let (current,document)=self.segment_document(&object.receipt).await?;
                    if reachability::object_identity(&current,&self.engine.repository)?!=reachability::object_identity(object,&self.engine.repository)? {return Err(ProviderError::new(ErrorKind::Corrupt))}Ok(document)},
                _=>self.documents.document(object).await,
            }
        })
    }
    fn listed<'a>(&'a self,receipt:&'a ObjectReceipt)->ProviderFuture<'a,(RemoteObject,DocumentNode)> {
        Box::pin(async move {
            let segment=self.known.lock().map_err(|_|ProviderError::new(ErrorKind::Transient))?.iter()
                .any(|object|object.role==ObjectRole::Segment && object.receipt.locator==receipt.locator);
            if segment {return self.segment_document(receipt).await}
            if let Some((object,document))=self.checkpoint_document(receipt).await? {
                return Ok((object,self.checkpoint_node(&document)?));
            }
            self.documents.listed(receipt).await
        })
    }
    fn catalog<'a>(&'a self,object:&'a RemoteObject)->ProviderFuture<'a,Vec<RemoteObject>> {
        if object.repository_id==self.lww_repository_id {
            Box::pin(control::read_catalog_children_for_repository(self.base.connected,object,&self.lww_repository_id,self.documents.cancel))
        } else {self.documents.catalog(object)}
    }
    fn metadata<'a>(&'a self,object:&'a RemoteObject)->ProviderFuture<'a,Option<ObjectReceipt>> {
        Box::pin(async move {
            if let Some(current)=self.verified(object)? {return Ok(Some(current))}
            if object.role!=ObjectRole::Segment && !self.native_body(object)? {
                return if object.repository_id==self.lww_repository_id {
                    self.documents.metadata_for_repository(object,&self.lww_repository_id).await
                } else {self.documents.metadata(object).await};
            }
            stored_metadata(self.engine.provider.as_ref(),&self.engine.repository,object,self.documents.cancel).await
        })
    }
    fn probe<'a>(&'a self,object:&'a RemoteObject)->ProviderFuture<'a,Option<ObjectReceipt>> {
        Box::pin(async move {
            if let Some(current)=self.verified(object)? {return Ok(Some(current))}
            if object.role!=ObjectRole::Segment && !self.native_body(object)? {
                return if object.repository_id==self.lww_repository_id {
                    self.documents.probe_for_repository(object,&self.lww_repository_id).await
                } else {self.documents.probe(object).await};
            }
            let intent=ObjectIntent{repository_id:self.engine.repository.repository_id.clone(),job_id:"cleanup-probe".into(),object_id:object.object_id.clone(),
                role:object.role,byte_length:object.receipt.byte_length,sha256:object.ciphertext_sha256.clone()};
            let mut receipt=object.receipt.clone();receipt.checksum=None;
            super::transfer_job::verify_remote_receipt(self.documents.scratch,&intent,self.engine.provider.as_ref(),&self.engine.repository,
                receipt,self.documents.cancel).await
        })
    }
}
/// Presence and length of a stored object from service metadata alone.
async fn stored_metadata(provider:&dyn Provider,repository:&RepositoryHandle,object:&RemoteObject,cancel:&Cancellation)->Result<Option<ObjectReceipt>> {
    let intent=ObjectIntent{repository_id:repository.repository_id.clone(),job_id:"cleanup-probe".into(),object_id:object.object_id.clone(),
        role:object.role,byte_length:object.receipt.byte_length,sha256:object.ciphertext_sha256.clone()};
    leases::control_request(cancel,provider.lookup_metadata(repository,&intent,Some(&object.receipt.locator),cancel)).await
}
impl ConnectedDocuments<'_> {
    fn metadata_for_repository<'a>(&'a self, object: &'a RemoteObject,repository_id:&'a str) -> ProviderFuture<'a, Option<ObjectReceipt>> {
        Box::pin(async move {
            object.stored(&self.connected.handle)?;
            if object.repository_id != repository_id {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            stored_metadata(self.connected.provider.as_ref(),&self.connected.handle,object,self.cancel).await
        })
    }
    fn probe_for_repository<'a>(&'a self, object: &'a RemoteObject,repository_id:&'a str) -> ProviderFuture<'a, Option<ObjectReceipt>> {
        Box::pin(async move {
            object.stored(&self.connected.handle)?;
            if object.repository_id != repository_id {
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
        self.probe_for_repository(object,&self.connected.stored.descriptor.repository_id)
    }
    fn metadata<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Option<ObjectReceipt>> {
        self.metadata_for_repository(object,&self.connected.stored.descriptor.repository_id)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::external_storage::{
        fake::{self, DeleteFault, FakeLeaseClock, FakeProvider},
        journal::JobIdentity,
        reachability::tests::{object, source, Source},
        transfer_job,
    };
    use crate::persistent_store::sync_selection::CaptureIdentity;
    use risunest_external_storage_format::format::{Descriptor, Strategy};
    use std::{io::Write, sync::{atomic::{AtomicUsize, Ordering}, Arc, Mutex}};

    /// An ordinary backup bundle published beside the checkpoints of `f`.
    async fn ordinary_backup(f:&super::super::lww_tests::CycleFixture,directory:&Path,cancel:&Cancellation)
        ->(ConnectedRepository,String,packaging::CompletedSnapshot) {
        use super::super::contract::{ConnectionConfig,RemoteLocator};
        let mut handle=fake::repository();handle.repository_id=f.sender.repository.repository_id.clone();
        let connected=ConnectedRepository {
            stored:super::super::connection_store::StoredConnection {
                id:"sender".into(),config:ConnectionConfig{provider:"synthetic".into(),profile:None,
                    endpoint:"https://synthetic.invalid".into(),account_id:"fixture".into(),location:BTreeMap::new(),oauth_profile:None},
                descriptor:f.sender.descriptor.clone(),descriptor_locator:RemoteLocator{connection_identity:f.sender.repository.connection_identity.clone(),collection:None,object:"descriptor".into()},
                provider_repository_id:f.sender.repository.repository_id.clone(),credential_ref:"credential".into(),root_key_ref:"key".into(),recovery_key_ref:"recovery".into(),
                retention_policy:None,transfer_concurrency:None,labels:Default::default(),capabilities:f.sender.capabilities.clone(),created_at_ms:1,verified_at_ms:1,last_sync_at_ms:None,last_backup_at_ms:None,
            },provider:f.provider.clone(),handle,
            dependencies:fake::loopback_dependencies(fake::MemoryVault::default(),1).dependencies,root_key:zeroize::Zeroizing::new([7;32]),
        };
        let cache=directory.join("cache");
        let source_root=f.directory_a.path().to_path_buf();let backup_spool=directory.join("backup-sections");
        let (capture,sections,original_units,writer)=super::super::worker_observation::spawn_blocking(move || {
            let mut source=crate::persistent_store::PersistentStore::open(&source_root).unwrap();
            let probe=super::super::runtime::CancelProbe(Cancellation::default());
            let hydration=source.hydrate_external_capture_dependencies("sender",&probe).unwrap();
            let (lease,prepared)=source.lww_acquire_backup_capture(source.revision().unwrap()).unwrap();
            let sections=super::super::sections::capture_prepared_backup_sections(&prepared,&backup_spool,&probe.0).unwrap();
            let capture=source.capture_external_library_from_lease_with_sections("sender",&hydration,&lease.lease,sections,&probe).unwrap();
            let sections=capture.catalog.backup_sections(&source_root).unwrap();let original=capture.catalog.original_backup_units().unwrap();
            (capture,sections,original,source.lww_clock_state().unwrap().writer_id)
        }).await.unwrap();
        let backup_id=uuid::Uuid::new_v4().to_string();
        let mut journal=TransferJournal::open(&directory.join("backup-journal"),super::super::journal::JobIdentity{
            job_id:backup_id.clone(),connection_id:"sender".into(),repository_id:connected.handle.repository_id.clone(),
            capture_id:capture.id.clone(),capture:capture.identity.clone(),
        }).unwrap();
        let metadata=packaging::SnapshotMetadata{snapshot_id:backup_id.clone(),repository_id:connected.stored.descriptor.repository_id.clone(),
            library_id:capture.identity.library_epoch.clone(),author_device_id:capture.identity.store_id.clone(),created_at_ms:super::super::runtime::now_ms(),
            logical_revision:capture.identity.revision as u64,parent_snapshot_id:None,
            content_fingerprint:capture.catalog.content_fingerprint(&risunest_external_storage_format::format::library_fingerprint_domain()).unwrap(),
            purpose:packaging::SnapshotPurpose::BackupBundle{source:BundleSource::Device{writer_id:writer},remote_generation:None,original_units},
        };
        let backup=packaging::package_and_upload(capture,sections,f.directory_a.path(),&cache,metadata,&connected.root_key,
            packaging::PackageLimits::from_capabilities(&connected.stored.capabilities).unwrap(),None,&mut journal,
            connected.provider.as_ref(),&connected.handle,&super::super::phase_progress::PhaseProgress::silent(),cancel).await.unwrap();
        (connected,backup_id,backup)
    }

    #[test]
    fn idle_receive_and_maintenance_read_each_snapshot_once_per_connection() {
        runtime().block_on(async {
            use super::super::lww_tests::CycleFixture;
            use crate::persistent_store::lww::ApplyReceive;
            use risunest_sync_wire::stamp::DecimalU64;
            let mut f=CycleFixture::new();let cancel=Cancellation::default();
            f.a.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:f.a.revision().unwrap(),unit_mutations:Some(vec![
                crate::persistent_store::lww::UnitMutation::Set{key:risunest_sync_wire::unit::UnitKey::new(&["root","language"]).unwrap(),value:serde_json::json!("en")},
            ]),..Default::default()}).unwrap();
            f.publish_a().await;
            let directory=tempfile::tempdir().unwrap();
            let writer=f.a.lww_clock_state().unwrap().writer_id;
            let first=f.sender.compact_published(&mut f.a,&directory.path().join("first"),"00000000-0000-4000-8000-000000000091",&writer,
                &f.sender.capabilities,&cancel,None).await.unwrap();
            let (_,_,backup)=ordinary_backup(&f,directory.path(),&cancel).await;
            assert_eq!(f.receiver.snapshot_listing(&cancel).await.unwrap().len(),1);
            let summaries=tokio::sync::Mutex::new(super::super::lww_compaction::CheckpointSummaries::default());
            let reads=|f:&CycleFixture|(f.provider.read_count(),f.provider.listing_count());
            let first_checkpoint=first.reference.receipt.locator.object.clone();
            let ordinary=backup.reference.receipt.locator.object.clone();
            let before=(f.provider.read_attempts(&first_checkpoint),f.provider.read_attempts(&ordinary));

            let requests=f.receiver.receive_requests_cached(&mut f.b,DecimalU64(0),&summaries,&cancel).await.unwrap().pages
                .iter().map(|id|f.b.external_lww_unfinished_receive(id).unwrap().unwrap()).collect::<Vec<_>>();
            assert!(!requests.is_empty());
            for request in requests {
                f.b.lww_stage_receive(&request).unwrap();
                f.b.lww_apply_receive(&ApplyReceive{header:request.header.clone(),generating:vec![]}).unwrap();
                f.b.lww_finish_receive(&request.header).unwrap();
            }
            assert_eq!((f.provider.read_attempts(&first_checkpoint),f.provider.read_attempts(&ordinary)),(before.0+1,before.1),
                "a receive reads the checkpoint once and never the backup stored beside it");

            let (read,listed)=reads(&f);
            assert!(f.receiver.receive_requests_cached(&mut f.b,DecimalU64(0),&summaries,&cancel).await.unwrap().pages.is_empty());
            assert_eq!(reads(&f),(read,listed+2),"an idle receive reads objects or lists more than twice");
            assert!(f.receiver.maintenance_needed_cached(&summaries,&cancel).await.unwrap().is_none());
            assert_eq!(reads(&f),(read,listed+4),"an idle maintenance check reads objects or lists more than twice");

            let own=tokio::sync::Mutex::new(super::super::lww_compaction::CheckpointSummaries::default());
            for id in f.sender.receive_requests_cached(&mut f.a,DecimalU64(0),&own,&cancel).await.unwrap().pages {
                let request=f.a.external_lww_unfinished_receive(&id).unwrap().unwrap();
                f.a.lww_stage_receive(&request).unwrap();
                f.a.lww_apply_receive(&ApplyReceive{header:request.header.clone(),generating:vec![]}).unwrap();
                f.a.lww_finish_receive(&request.header).unwrap();
            }
            let (read,listed)=reads(&f);
            assert!(f.sender.receive_requests_cached(&mut f.a,DecimalU64(0),&own,&cancel).await.unwrap().pages.is_empty());
            assert_eq!(reads(&f),(read,listed+2),"an idle receive on the publishing device reads objects or lists more than twice");

            let second=f.sender.compact_published(&mut f.a,&directory.path().join("second"),"00000000-0000-4000-8000-000000000092",&writer,
                &f.sender.capabilities,&cancel,None).await.unwrap();
            let (read,_)=reads(&f);
            let known=(f.provider.read_attempts(&first_checkpoint),f.provider.read_attempts(&ordinary));
            assert!(f.receiver.receive_requests_cached(&mut f.b,DecimalU64(0),&summaries,&cancel).await.unwrap().pages.is_empty());
            assert_eq!(reads(&f).0,read+1,"a new checkpoint is the only object the next receive reads");
            assert_eq!(f.provider.read_attempts(&second.reference.receipt.locator.object),1);
            assert_eq!((f.provider.read_attempts(&first_checkpoint),f.provider.read_attempts(&ordinary)),known);
        });
    }

    #[test]
    fn a_recheck_names_a_backup_point_by_the_receipt_listed_now() {
        runtime().block_on(async {
            use super::super::lww_tests::CycleFixture;
            let mut f=CycleFixture::new();let cancel=Cancellation::default();
            f.a.commit(&crate::persistent_store::WorkingSetCommit{expected_revision:f.a.revision().unwrap(),unit_mutations:Some(vec![
                crate::persistent_store::lww::UnitMutation::Set{key:risunest_sync_wire::unit::UnitKey::new(&["root","language"]).unwrap(),value:serde_json::json!("en")},
            ]),..Default::default()}).unwrap();
            f.publish_a().await;
            let directory=tempfile::tempdir().unwrap();
            let (connected,backup_id,backup)=ordinary_backup(&f,directory.path(),&cancel).await;
            let mut journal=TransferJournal::open(&directory.path().join("point-journal"),JobIdentity{
                job_id:"keep".into(),connection_id:"sender".into(),repository_id:connected.handle.repository_id.clone(),capture_id:backup_id,
                capture:CaptureIdentity{store_id:"store".into(),library_epoch:"epoch".into(),generation:"generation".into(),selection_epoch:"selection".into(),revision:1},
            }).unwrap();
            let document=control::BackupPointDocument::single(&connected.stored.descriptor,"kept".into(),control::BackupPointKind::Manual,1000,backup.reference.clone()).unwrap();
            let point=control::upload_backup_point(&connected.stored.descriptor,&connected.root_key,document,&mut journal,
                connected.provider.as_ref(),&connected.handle,&cancel).await.unwrap();
            let cache=directory.path().join("cache");let scratch=directory.path().join("probe");
            let sender_store=std::sync::Mutex::new(f.a.open_native_job_store().unwrap());
            let view=LwwRepositoryView::new(&f.sender,&sender_store,ConnectedRepositoryView{connected:&connected,writer_id:"collector",policy:RetentionPolicy::DEFAULT,
                now_ms:1000,unfinished:vec![],cache_root:&cache,reads:Default::default()},ConnectedDocuments{connected:&connected,cancel:&cancel,scratch:&scratch});
            let first=view.roots(&cancel).await.unwrap().points["kept"].receipt.clone();
            // The same name listed again under another version is a point this view has not read.
            f.provider.seed(&point.object_id,ObjectRole::BackupPoint,f.provider.contents(&point.object_id).unwrap());
            let listed=connected.provider.list_objects(&connected.handle,Collection::BackupPoints,None,100,&cancel).await.unwrap()
                .objects.into_iter().find(|receipt|receipt.locator==first.locator).unwrap();
            assert_ne!(listed,first);
            assert_eq!(view.roots(&cancel).await.unwrap().points["kept"].receipt,listed);
        });
    }

    #[test]
    fn lww_gc_retires_native_body_before_its_signed_discovery_parent() {
        for repository_id in super::super::lww_tests::representative_repository_ids() {
        runtime().block_on(async {
            use super::super::{lww_tests::{CycleFixture,small_asset},fake};
            let mut f=CycleFixture::with_repository_id(&repository_id);let cancel=Cancellation::default();
            small_asset(&mut f.a,"obsolete-large",&vec![31;5*1024*1024]);
            let mut small=Vec::new();
            for n in 0..4 {small.push(small_asset(&mut f.a,&format!("small-{n}"),format!("synthetic retained pack entry {n}").as_bytes()));}
            f.publish_a().await;
            f.a.delete_asset_alias("asset","obsolete-large",f.a.revision().unwrap()).unwrap();
            for n in 1..4 {f.a.delete_asset_alias("asset",&format!("small-{n}"),f.a.revision().unwrap()).unwrap();}
            f.publish_a().await;
            let directory=tempfile::tempdir().unwrap();
            let a_writer=f.a.lww_clock_state().unwrap().writer_id;
            let baseline=f.receiver.compact_published(&mut f.b,&directory.path().join("remote-baseline"),"ffffffff-ffff-4fff-8fff-ffffffffffff",&a_writer,
                &f.receiver.capabilities,&cancel,None).await.unwrap();
            assert!(f.receive_b().await>0);
            let saved=super::super::lww_residency::packed_source(f.directory_b.path(),&small[0]).unwrap().unwrap();
            let old_pack=saved.packs[0].header.object_id.clone();
            assert!(crate::asset_repository::PayloadCas::new(f.directory_b.path()).unwrap().stat_object(&small[0]).unwrap().is_none());
            let id="00000000-0000-4000-8000-000000000090".to_owned();
            let checkpoint=f.sender.compact_published(&mut f.a,directory.path(),&id,&a_writer,
                &f.sender.capabilities,&cancel,None).await.unwrap();
            let old=f.sender.checkpoint(&baseline.reference.receipt,&cancel).await.unwrap().1;
            let current=f.sender.checkpoint(&checkpoint.reference.receipt,&cancel).await.unwrap().1;
            assert_eq!(old.covered_prefixes,current.covered_prefixes);
            assert_eq!(checkpoint.reference.role,ObjectRole::Snapshot);
            assert_eq!(checkpoint.reference.object_id,id);
            let (connected,backup_id,backup)=ordinary_backup(&f,directory.path(),&cancel).await;
            let cache=directory.path().join("cache");let scratch=directory.path().join("probe");
            assert_ne!(backup.reference.repository_id,f.sender.repository.repository_id);
            assert_eq!(f.sender.snapshot_listing(&cancel).await.unwrap().len(),2,"ordinary logical-scope backup coexists with the physical checkpoints");
            let sender_store=std::sync::Mutex::new(f.a.open_native_job_store().unwrap());
            let view=LwwRepositoryView::new(&f.sender,&sender_store,ConnectedRepositoryView{connected:&connected,writer_id:"sender",policy:RetentionPolicy::DEFAULT,
                now_ms:1000,unfinished:vec![],cache_root:&cache,reads:Default::default()},ConnectedDocuments{connected:&connected,cancel:&cancel,scratch:&scratch});
            let observed=view.roots(&cancel).await.unwrap();let listed=view.snapshots(&cancel).await.unwrap();let known=view.known_objects().unwrap();
            let inventory=view.inventory(&cancel).await.unwrap();
            assert!(inventory.pages.iter().any(|page|page.reference.repository_id==checkpoint.reference.repository_id));
            assert!(inventory.pages.iter().any(|page|page.reference.repository_id==connected.stored.descriptor.repository_id));
            assert!(inventory.objects.iter().any(|object|object.object_id==id && object.role==ObjectRole::Snapshot));
            assert_eq!(backup.reference.role,ObjectRole::BackupBundle);
            assert_eq!(backup.reference.object_id,format!("snapshot-{backup_id}"));
            let body=known.iter().find(|object|view.native_body(object).unwrap()).unwrap().clone();
            assert!(body.stored(&connected.handle).is_err(),"native ciphertext is not a generic StoredObject envelope");
            assert!(validate_generic_body(&body,&connected.handle).is_err());
            let roots=Roots{head:observed.head,kept_points:observed.kept_points,kept_bundles:observed.kept_bundles,..Default::default()};
            let first=reachability::mark(f.directory_a.path(),&view,reachability::MarkRequest{connection_id:"sender",repository:&connected.handle,
                format_repository_id:&connected.stored.descriptor.repository_id,now_ms:1000,roots:roots.clone(),listed:listed.clone(),known_objects:known.clone(),retired_points:vec![]},&cancel).await.unwrap();
            assert!(first.candidates.is_empty());
            let marked=reachability::mark(f.directory_a.path(),&view,reachability::MarkRequest{connection_id:"sender",repository:&connected.handle,
                format_repository_id:&connected.stored.descriptor.repository_id,now_ms:1000+leases::UNREACHABLE_GRACE_MS,roots:roots.clone(),listed:listed.clone(),known_objects:known.clone(),retired_points:vec![]},&cancel).await.unwrap();
            assert_eq!(marked.candidates.first().unwrap().object_id,body.object_id);
            assert!(marked.candidates.iter().skip(1).any(|object|object.role==ObjectRole::Segment));
            let mut protected=roots;protected.job_references.push(body.clone());
            let retained=reachability::mark(f.directory_a.path(),&view,reachability::MarkRequest{connection_id:"sender",repository:&connected.handle,
                format_repository_id:&connected.stored.descriptor.repository_id,now_ms:1000+leases::UNREACHABLE_GRACE_MS,
                roots:protected,listed,known_objects:known,retired_points:vec![]},&cancel).await.unwrap();
            assert!(!retained.candidates.iter().any(|object|object.object_id==body.object_id));
            assert!(retained.candidates.iter().any(|object|object.role==ObjectRole::Segment),"a live body does not pin obsolete discovery parents");
            let clock=fake::FakeLeaseClock::new(1000+leases::UNREACHABLE_GRACE_MS);
            let context=LeaseContext{root:f.directory_a.path(),connection_id:"sender",writer_id:"collector",descriptor:&connected.stored.descriptor,
                root_key:&connected.root_key,provider:connected.provider.as_ref(),repository:&connected.handle,clock:&clock,protection_supported:true,ledger:None};
            let request=CleanupRequest{job_id:"first-finite-gc",cleanup_supported:true,limits:CleanupLimits{batch:1,per_run:1},connection_time:&available_time};
            let first=run_lww(&context,&request,&f.sender,&sender_store,ConnectedRepositoryView{connected:&connected,writer_id:"collector",policy:RetentionPolicy::DEFAULT,
                now_ms:1000+leases::UNREACHABLE_GRACE_MS,unfinished:vec![],cache_root:&cache,reads:Default::default()},&scratch,&cancel).await.unwrap();
            assert!(first.deleted_objects<=1);assert!(f.provider.holds(&body.object_id),"a newly unmarked protected body retains its grace period");
            clock.advance(leases::UNREACHABLE_GRACE_MS);
            let request=CleanupRequest{job_id:"second-finite-gc",..request};
            let settled=run_lww(&context,&request,&f.sender,&sender_store,ConnectedRepositoryView{connected:&connected,writer_id:"collector",policy:RetentionPolicy::DEFAULT,
                now_ms:1000+2*leases::UNREACHABLE_GRACE_MS,unfinished:vec![],cache_root:&cache,reads:Default::default()},&scratch,&cancel).await.unwrap();
            assert_eq!(settled.deleted_objects,1);assert!(!f.provider.holds(&body.object_id));
            assert!(view.snapshots(&cancel).await.unwrap().len()>=3,"partial deletion keeps signed discovery parents");
            let request=CleanupRequest{job_id:"retire-covered-pack",limits:CleanupLimits{batch:100,per_run:100},..request};
            run_lww(&context,&request,&f.sender,&sender_store,ConnectedRepositoryView{connected:&connected,writer_id:"collector",policy:RetentionPolicy::DEFAULT,
                now_ms:1000+2*leases::UNREACHABLE_GRACE_MS,unfinished:vec![],cache_root:&cache,reads:Default::default()},&scratch,&cancel).await.unwrap();
            assert!(!f.provider.holds(&old_pack),"finite aged GC retires the saved equal-coverage pack");
            let mut receiver_connection=ConnectedRepository{stored:connected.stored.clone(),provider:connected.provider.clone(),handle:fake::repository(),
                dependencies:fake::loopback_dependencies(fake::MemoryVault::default(),1).dependencies,root_key:zeroize::Zeroizing::new(*connected.root_key)};
            receiver_connection.handle.repository_id=repository_id.clone();
            receiver_connection.stored.id="receiver".into();
            let usage=super::super::lww_residency::usage_receipts(f.directory_b.path(),&receiver_connection.stored).unwrap();
            assert!(usage.iter().any(|receipt|receipt.locator.object==saved.packs[0].locator.object));
            super::super::connection_store::ConnectionStore::open(f.directory_b.path()).unwrap().insert(&receiver_connection.stored).unwrap();
            let _resolver=super::super::lww_residency::install_test_source_connection(f.directory_b.path(),Arc::new(receiver_connection)).unwrap();
            let catalog_id=&current.asset_catalogs[0].header.object_id;
            let authentic=f.provider.state.lock().unwrap().objects[catalog_id].0.clone();
            let mut corrupt=authentic.clone();let last=corrupt.len()-1;corrupt[last]^=1;
            f.provider.seed(catalog_id,ObjectRole::Catalog,corrupt);
            let root=f.directory_b.path().to_path_buf();let hash=small[0].clone();
            super::super::worker_observation::spawn_blocking(move || {
                assert!(super::super::lww_residency::hydrate_registered(&root,&hash,&||Ok(())).is_err());
                assert!(crate::asset_repository::PayloadCas::new(&root).unwrap().stat_object(&hash).unwrap().is_none());
            }).await.unwrap();
            assert_eq!(super::super::lww_residency::packed_source(f.directory_b.path(),&small[0]).unwrap().unwrap().packs,saved.packs);
            f.provider.seed(catalog_id,ObjectRole::Catalog,authentic);
            let frozen=super::super::lww_residency::FrozenBodySource::Packed(saved.clone());
            let downloaded=Arc::new(std::sync::atomic::AtomicU64::new(0));let observed=downloaded.clone();
            let before_missing=f.provider.read_attempts(&old_pack);
            let spool=super::super::lww_residency::spool_frozen_remote_body_observed(&frozen,&scratch.join("frozen-destination"),&cancel,Some(Arc::new(move |bytes| {
                assert!(bytes>0);observed.fetch_add(bytes,std::sync::atomic::Ordering::Relaxed);
            }))).await.unwrap();
            let received=downloaded.load(std::sync::atomic::Ordering::Relaxed);
            assert!(received>0,"the refreshed packed source must report downloaded bytes");
            assert_eq!(f.provider.read_attempts(&old_pack),before_missing+1);
            assert_eq!(std::fs::read(spool.path()).unwrap(),b"synthetic retained pack entry 0");
            assert_eq!(downloaded.load(std::sync::atomic::Ordering::Relaxed),received,"reading the local spool is not another download");
            assert!(crate::asset_repository::PayloadCas::new(f.directory_b.path()).unwrap().stat_object(&small[0]).unwrap().is_none());
            assert_eq!(super::super::lww_residency::packed_source(f.directory_b.path(),&small[0]).unwrap().unwrap().packs,saved.packs,"frozen backup custody never switches its saved source identity");
            let root=f.directory_b.path().to_path_buf();let hash=small[0].clone();
            super::super::worker_observation::spawn_blocking(move || {
                assert!(super::super::lww_residency::hydrate_registered(&root,&hash,&||Ok(())).unwrap());
                assert_eq!(crate::asset_repository::PayloadCas::new(&root).unwrap().read_object(&hash).unwrap().unwrap(),b"synthetic retained pack entry 0");
            }).await.unwrap();
            let refreshed=super::super::lww_residency::packed_source(f.directory_b.path(),&small[0]).unwrap().unwrap();
            assert_eq!(refreshed.protected_snapshot,id);assert_ne!(refreshed.packs[0].header.object_id,old_pack);
            assert_eq!(received,refreshed.packs.iter().map(|pack|pack.ciphertext_length).sum::<u64>());
        });
        }
    }
    fn validate_generic_body(object:&RemoteObject,repository:&RepositoryHandle)->Result<()> {object.stored(repository).map(|_|())}

    /// Runs cleanup on the sender's repository twice, the grace period apart,
    /// so the second run removes what the first found unreachable.
    pub(crate) async fn collect_after_grace(f:&super::super::lww_tests::CycleFixture,directory:&Path)->(CleanupOutcome,CleanupOutcome) {
        let cancel=Cancellation::default();
        let connected=fixture_connection(f);
        let sender_store=std::sync::Mutex::new(f.a.open_native_job_store().unwrap());
        let cache=directory.join("cache");let scratch=directory.join("probe");
        let start=super::super::runtime::now_ms();
        let clock=fake::FakeLeaseClock::new(start);
        let context=LeaseContext{root:f.directory_a.path(),connection_id:"sender",writer_id:"collector",descriptor:&connected.stored.descriptor,
            root_key:&connected.root_key,provider:connected.provider.as_ref(),repository:&connected.handle,clock:&clock,protection_supported:true,ledger:None};
        let request=CleanupRequest{job_id:"mark-unreachable",cleanup_supported:true,limits:CleanupLimits::default(),connection_time:&available_time};
        let first=run_lww(&context,&request,&f.sender,&sender_store,ConnectedRepositoryView{connected:&connected,writer_id:"collector",policy:RetentionPolicy::DEFAULT,
            now_ms:start,unfinished:vec![],cache_root:&cache,reads:Default::default()},&scratch,&cancel).await.unwrap();
        clock.advance(leases::UNREACHABLE_GRACE_MS);
        let request=CleanupRequest{job_id:"remove-unreachable",..request};
        let second=run_lww(&context,&request,&f.sender,&sender_store,ConnectedRepositoryView{connected:&connected,writer_id:"collector",policy:RetentionPolicy::DEFAULT,
            now_ms:start+leases::UNREACHABLE_GRACE_MS,unfinished:vec![],cache_root:&cache,reads:Default::default()},&scratch,&cancel).await.unwrap();
        (first,second)
    }

    #[test]
    fn versions_of_segments_cleanup_removed_under_a_checkpoint_are_released() {
        runtime().block_on(async {
            use super::super::lww_tests::{CycleFixture,device_rows};
            use crate::persistent_store::{lww::UnitMutation,WorkingSetCommit};
            let mut f=CycleFixture::new();let cancel=Cancellation::default();
            for value in ["one","two","three"] {
                f.a.commit(&WorkingSetCommit{expected_revision:f.a.revision().unwrap(),unit_mutations:Some(vec![UnitMutation::Set{
                    key:risunest_sync_wire::unit::UnitKey::new(&["root","language"]).unwrap(),value:serde_json::json!(value)}]),..Default::default()}).unwrap();
                f.publish_a().await;
            }
            assert!(device_rows(&f.a,"external_lww_versions")>=3);
            let directory=tempfile::tempdir().unwrap();
            let writer=f.a.lww_clock_state().unwrap().writer_id;
            f.sender.compact_published(&mut f.a,&directory.path().join("compaction"),"00000000-0000-4000-8000-0000000000e1",
                &writer,&f.sender.capabilities,&cancel,None).await.unwrap();
            assert!(device_rows(&f.a,"external_lww_versions")>=3,"compaction releases no versions");
            let (_,removed)=collect_after_grace(&f,directory.path()).await;
            assert_eq!(removed.stop_reason,StopReason::Complete);
            assert!(f.sender.listing(&cancel).await.unwrap().is_empty(),"the covered segments are removed");
            assert_eq!(device_rows(&f.a,"external_lww_versions"),0,"versions of removed covered segments stay");
        });
    }

    #[test]
    fn lww_cleanup_answers_segments_this_device_published_from_its_reference_cache() {
        runtime().block_on(async {
            use super::super::lww_tests::CycleFixture;
            use crate::persistent_store::{lww::UnitMutation,WorkingSetCommit};
            let mut f=CycleFixture::new();
            for value in ["one","two","three"] {
                f.a.commit(&WorkingSetCommit{expected_revision:f.a.revision().unwrap(),unit_mutations:Some(vec![UnitMutation::Set{
                    key:risunest_sync_wire::unit::UnitKey::new(&["root","language"]).unwrap(),value:serde_json::json!(value)}]),..Default::default()}).unwrap();
                f.publish_a().await;
            }
            let segments=f.sender.listing(&Cancellation::default()).await.unwrap().into_iter().map(|receipt|receipt.locator.object).collect::<Vec<_>>();
            let reads=|f:&CycleFixture|segments.iter().map(|id|f.provider.read_attempts(id)).collect::<Vec<_>>();
            let before=reads(&f);
            let directory=tempfile::tempdir().unwrap();
            let (first,second)=collect_after_grace(&f,directory.path()).await;
            assert_eq!((first.deleted_objects,second.deleted_objects),(0,0));
            assert_eq!(reads(&f),before,"cleanup downloads segments this device published");
        });
    }

    pub(crate) fn fixture_connection(f:&super::super::lww_tests::CycleFixture)->ConnectedRepository {
        use super::super::contract::{ConnectionConfig,RemoteLocator};
        ConnectedRepository {
            stored:super::super::connection_store::StoredConnection {
                id:"sender".into(),config:ConnectionConfig{provider:"synthetic".into(),profile:None,
                    endpoint:"https://synthetic.invalid".into(),account_id:"fixture".into(),location:BTreeMap::new(),oauth_profile:None},
                descriptor:f.sender.descriptor.clone(),descriptor_locator:RemoteLocator{connection_identity:f.sender.repository.connection_identity.clone(),collection:None,object:"descriptor".into()},
                provider_repository_id:f.sender.repository.repository_id.clone(),credential_ref:"credential".into(),root_key_ref:"key".into(),recovery_key_ref:"recovery".into(),
                retention_policy:None,transfer_concurrency:None,labels:Default::default(),capabilities:f.sender.capabilities.clone(),created_at_ms:1,verified_at_ms:1,last_sync_at_ms:None,last_backup_at_ms:None,
            },provider:f.provider.clone(),handle:fake::repository(),
            dependencies:fake::loopback_dependencies(fake::MemoryVault::default(),1).dependencies,root_key:zeroize::Zeroizing::new([7;32]),
        }
    }

    #[test]
    fn lww_cleanup_reads_a_segment_only_when_this_device_has_no_references_for_it() {
        runtime().block_on(async {
            use super::super::lww_tests::CycleFixture;
            use crate::persistent_store::{lww::UnitMutation,WorkingSetCommit};
            let mut f=CycleFixture::new();let cancel=Cancellation::default();
            let set=|store:&mut crate::persistent_store::PersistentStore,key:&str,value:serde_json::Value| {
                store.commit(&WorkingSetCommit{expected_revision:store.revision().unwrap(),unit_mutations:Some(vec![UnitMutation::Set{
                    key:risunest_sync_wire::unit::UnitKey::new(&["root",key]).unwrap(),value}]),..Default::default()}).unwrap();
            };
            for value in ["one","two","three"] {set(&mut f.a,"language",serde_json::json!(value));f.publish_a().await;}
            let directory=tempfile::tempdir().unwrap();
            let writer=f.a.lww_clock_state().unwrap().writer_id;
            let completed=f.sender.compact_published(&mut f.a,&directory.path().join("compaction"),"00000000-0000-4000-8000-0000000000a1",
                &writer,&f.sender.capabilities,&cancel,None).await.unwrap();
            set(&mut f.a,"language",serde_json::json!("four"));f.publish_a().await;
            set(&mut f.b,"loreBookDepth",serde_json::json!(3));
            f.receiver.publish(&mut f.b,risunest_sync_wire::stamp::DecimalU64(0),&[],&cancel).await.unwrap();
            let segments=f.sender.listing(&cancel).await.unwrap().into_iter().map(|receipt|receipt.locator.object).collect::<Vec<_>>();
            assert_eq!(segments.len(),5);
            let foreign=segments.iter().filter(|id|super::super::contract::parse_segment_object_id(id).unwrap().0!=writer).cloned().collect::<Vec<_>>();
            assert_eq!(foreign.len(),1);
            let checkpoint=completed.reference.receipt.locator.object.clone();
            let reads=|f:&CycleFixture,ids:&[String]|ids.iter().map(|id|f.provider.read_attempts(id)).collect::<Vec<_>>();
            let connected=fixture_connection(&f);
            let cache=directory.path().join("cache");let scratch=directory.path().join("probe");
            let clock=fake::FakeLeaseClock::new(super::super::runtime::now_ms());
            let context=LeaseContext{root:f.directory_a.path(),connection_id:"sender",writer_id:"collector",descriptor:&connected.stored.descriptor,
                root_key:&connected.root_key,provider:connected.provider.as_ref(),repository:&connected.handle,clock:&clock,protection_supported:true,ledger:None};
            let sender_store=std::sync::Mutex::new(f.a.open_native_job_store().unwrap());
            for run in 0..2 {
                let (segments_before,checkpoint_before)=(reads(&f,&segments),f.provider.read_attempts(&checkpoint));
                let request=CleanupRequest{job_id:if run==0 {"idle-cleanup"} else {"idle-cleanup-again"},cleanup_supported:true,limits:CleanupLimits::default(),connection_time:&available_time};
                let outcome=run_lww(&context,&request,&f.sender,&sender_store,ConnectedRepositoryView{connected:&connected,writer_id:"collector",policy:RetentionPolicy::DEFAULT,
                    now_ms:super::super::runtime::now_ms(),unfinished:vec![],cache_root:&cache,reads:Default::default()},&scratch,&cancel).await.unwrap();
                assert_eq!(outcome.stop_reason,StopReason::Complete);
                assert_eq!(outcome.deleted_objects,0);
                for ((id,before),after) in segments.iter().zip(&segments_before).zip(reads(&f,&segments)) {
                    let expected=usize::from(run==0 && foreign.contains(id));
                    assert_eq!(after-before,expected,"segment {id} in run {run}");
                }
                assert!(f.provider.read_attempts(&checkpoint)-checkpoint_before<=1);
            }
        });
    }

    #[test]
    fn lww_cleanup_checks_live_bodies_without_downloading_them() {
        runtime().block_on(async {
            use super::super::lww_tests::{CycleFixture,small_asset};
            let mut f=CycleFixture::new();let cancel=Cancellation::default();
            small_asset(&mut f.a,"live-large",&vec![37;5*1024*1024]);
            for n in 0..3 {small_asset(&mut f.a,&format!("live-{n}"),format!("synthetic live pack entry {n}").as_bytes());}
            f.publish_a().await;
            let directory=tempfile::tempdir().unwrap();
            let writer=f.a.lww_clock_state().unwrap().writer_id;
            let completed=f.sender.compact_published(&mut f.a,&directory.path().join("compaction"),"00000000-0000-4000-8000-0000000000a2",
                &writer,&f.sender.capabilities,&cancel,None).await.unwrap();
            let (_,checkpoint)=f.sender.checkpoint(&completed.reference.receipt,&cancel).await.unwrap();
            let standalone=checkpoint.standalone_bodies.values().map(|body|body.locator.clone().unwrap().object).collect::<Vec<_>>();
            assert_eq!(standalone.len(),1);
            let packs=f.provider.objects_with_role(ObjectRole::Pack);
            assert!(packs.len()>standalone.len() && standalone.iter().all(|id|packs.contains(id)),"packed and standalone bodies are live");
            let catalogs=f.provider.objects_with_role(ObjectRole::Catalog);
            assert!(!catalogs.is_empty());
            let reads=|f:&CycleFixture,ids:&[String]|ids.iter().map(|id|f.provider.read_attempts(id)).collect::<Vec<_>>();
            let reconciles=|f:&CycleFixture,ids:&[String]|ids.iter().map(|id|f.provider.reconcile_attempts(id)).collect::<Vec<_>>();
            let (packs_before,catalogs_before,reconciled)=(reads(&f,&packs),reads(&f,&catalogs),reconciles(&f,&packs));
            let connected=fixture_connection(&f);
            let cache=directory.path().join("cache");let scratch=directory.path().join("probe");
            let clock=fake::FakeLeaseClock::new(super::super::runtime::now_ms());
            let context=LeaseContext{root:f.directory_a.path(),connection_id:"sender",writer_id:"collector",descriptor:&connected.stored.descriptor,
                root_key:&connected.root_key,provider:connected.provider.as_ref(),repository:&connected.handle,clock:&clock,protection_supported:true,ledger:None};
            let request=CleanupRequest{job_id:"idle-cleanup",cleanup_supported:true,limits:CleanupLimits::default(),connection_time:&available_time};
            let sender_store=std::sync::Mutex::new(f.a.open_native_job_store().unwrap());
            let outcome=run_lww(&context,&request,&f.sender,&sender_store,ConnectedRepositoryView{connected:&connected,writer_id:"collector",policy:RetentionPolicy::DEFAULT,
                now_ms:super::super::runtime::now_ms(),unfinished:vec![],cache_root:&cache,reads:Default::default()},&scratch,&cancel).await.unwrap();
            assert_eq!(outcome.stop_reason,StopReason::Complete);
            assert_eq!(outcome.deleted_objects,0);
            assert_eq!(reads(&f,&packs),packs_before,"no live pack or standalone body is downloaded");
            for ((id,before),after) in catalogs.iter().zip(&catalogs_before).zip(reads(&f,&catalogs)) {
                assert!(after-before<=1,"catalog {id} is read once per cleanup run");
            }
            for id in &standalone {assert!(f.provider.metadata_attempts(id)>0,"a live standalone body is still checked");}
            assert_eq!(reconciles(&f,&packs),reconciled,"the inventory survey reads metadata only");
        });
    }

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
        removed: Mutex<Vec<String>>,
        inventory: Mutex<InventorySurvey>,
        retired_inventory: Mutex<Vec<String>>,
        protected: Mutex<Vec<String>>,
        inventory_delete_error: Mutex<Option<ErrorKind>>,
        surveys: AtomicUsize,
        /// What a page reads when its retirement is confirmed, after the survey listed it.
        confirmations: Mutex<Vec<InventoryPageState>>,
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
        fn inventory<'a>(&'a self, _: &'a Cancellation) -> ProviderFuture<'a, InventorySurvey> {
            Box::pin(async move {
                self.surveys.fetch_add(1, Ordering::SeqCst);
                Ok(self.inventory.lock().unwrap().clone())
            })
        }
        fn inventory_page<'a>(&'a self, page: &'a RemoteObject, _: &'a Cancellation) -> ProviderFuture<'a, Option<InventoryPageState>> {
            Box::pin(async move {
                if let Some(changed) = self.confirmations.lock().unwrap().iter().find(|current| current.reference.object_id == page.object_id) {
                    return Ok(Some(changed.clone()));
                }
                Ok(self.inventory.lock().unwrap().pages.iter().find(|current| current.reference.object_id == page.object_id).cloned())
            })
        }
        fn delete_inventory_page<'a>(
            &'a self,
            expected: &'a risunest_external_storage_format::snapshot::StoredObject,
            _: &'a Cancellation,
        ) -> ProviderFuture<'a, control::RemoteInventoryPageDeleteOutcome> {
            Box::pin(async move {
                self.retired_inventory.lock().unwrap().push(expected.header.object_id.clone());
                if let Some(kind) = *self.inventory_delete_error.lock().unwrap() {
                    return Err(ProviderError::new(kind));
                }
                Ok(control::RemoteInventoryPageDeleteOutcome::Deleted)
            })
        }
        fn confirmed_removed(&self, object: &RemoteObject) -> Result<()> {
            self.removed.lock().unwrap().push(object.object_id.clone());
            Ok(())
        }
        fn protected_jobs(&self) -> Vec<String> { self.protected.lock().unwrap().clone() }
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
            Box::pin(async move {
                self.tick();
                if !self.provider.holds(&object.object_id) { return Err(ProviderError::new(ErrorKind::NotFound)); }
                self.source.catalog(object).await
            })
        }
        fn probe<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Option<ObjectReceipt>> {
            Box::pin(async move {
                self.tick();
                let receipt = self.source.probe(object).await?;
                Ok(if self.provider.holds(&object.object_id) { receipt } else { None })
            })
        }
        fn metadata<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Option<ObjectReceipt>> {
            Box::pin(async move {
                self.tick();
                let receipt = self.source.metadata(object).await?;
                Ok(if self.provider.holds(&object.object_id) { receipt } else { None })
            })
        }
    }
    impl Probes<'_> {
        /// The first remote check of a run takes the time a test asks for.
        fn tick(&self) {
            self.clock.advance(self.advance_once.swap(0, Ordering::SeqCst) as u64);
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
                    listing_error: Mutex::new(None), removed: Mutex::new(Vec::new()),
                    inventory: Mutex::new(InventorySurvey::default()),
                    retired_inventory: Mutex::new(Vec::new()),
                    protected: Mutex::new(Vec::new()),
                    inventory_delete_error: Mutex::new(None),
                    surveys: AtomicUsize::new(0),
                    confirmations: Mutex::new(Vec::new()),
                },
            }
        }
        fn context(&self) -> LeaseContext<'_> {
            LeaseContext {
                root: self.directory.path(), connection_id: "connection", writer_id: "writer",
                descriptor: &self.descriptor, root_key: &[7; 32], provider: &self.provider,
                repository: &self.repository, clock: &self.clock, protection_supported: true,
                ledger: None,
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
    fn unfinished_provider_upload_is_removed_only_after_the_live_lease_releases() {
        runtime().block_on(async {
            let h=Harness::new();
            let owner=match leases::admit(&h.context(),"active-upload",LeaseKind::Work,&Cancellation::default()).await.unwrap() {
                Admission::Admitted(owner)=>owner,_=>panic!("work must be admitted"),
            };
            h.provider.seed("unfinished-pack",ObjectRole::Pack,Vec::new());
            assert_eq!(h.run().await.unwrap().stop_reason,StopReason::Lease);
            assert!(h.provider.holds("unfinished-pack"));
            owner.release_all(&h.context()).await;
            assert_eq!(h.run().await.unwrap().deleted_objects,1);
            assert!(!h.provider.holds("unfinished-pack"));
        });
    }

    #[test]
    fn incomplete_scan_timeout_releases_protection_but_uncertain_delete_retains_it() {
        runtime().block_on(async {
            tokio::time::pause();
            let h = Harness::new();
            h.provider.seed("unfinished-pack", ObjectRole::Pack, Vec::new());
            h.provider.state.lock().unwrap().incomplete_cleanup_delay = Some(Duration::from_secs(61));
            assert_eq!(h.run().await.unwrap_err().kind, ErrorKind::Transient);
            assert!(h.provider.holds("unfinished-pack"));
            assert!(h.provider.objects_with_role(ObjectRole::Lease).is_empty());
            h.provider.state.lock().unwrap().incomplete_cleanup_delay = Some(Duration::from_secs(31));
            h.provider.state.lock().unwrap().incomplete_cleanup_empty_pages = 2;
            let started = tokio::time::Instant::now();
            assert_eq!(h.run().await.unwrap().deleted_objects, 1);
            assert!(started.elapsed() > Duration::from_secs(60));
            assert!(!h.provider.holds("unfinished-pack"));
            h.provider.state.lock().unwrap().incomplete_cleanup_delay = None;
            h.provider.state.lock().unwrap().incomplete_cleanup_empty_pages = 0;

            h.provider.seed("unfinished-lost", ObjectRole::Pack, Vec::new());
            h.provider.fail_delete("unfinished-lost", DeleteFault::AppliedThenLost);
            assert_eq!(h.run().await.unwrap_err().kind, ErrorKind::Transient);
            assert!(!h.provider.holds("unfinished-lost"));
            assert!(!h.provider.objects_with_role(ObjectRole::Lease).is_empty());
            assert_eq!(h.run().await.unwrap().stop_reason, StopReason::Lease);
        });
    }

    #[test]
    fn idle_lww_cleanup_runs_only_after_compaction_or_a_long_interval() {
        let start = NOW;
        assert!(lww_cleanup_due(false, None, start));
        assert!(!lww_cleanup_due(false, Some(start), start + 60_000));
        assert!(!lww_cleanup_due(false, Some(start), start + LWW_CLEANUP_INTERVAL_MS - 1));
        assert!(lww_cleanup_due(false, Some(start), start + LWW_CLEANUP_INTERVAL_MS));
        assert!(lww_cleanup_due(true, Some(start), start + 60_000));
        assert!(lww_cleanup_due(false, Some(start), start - 1), "a clock moved backward does not postpone cleanup");
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
    fn c_empty_inventory_pages_wait_seven_days_and_are_rechecked_before_retirement() {
        runtime().block_on(async {
            let h = Harness::new();
            let pages = ["inventory-page-old", "inventory-page-older", "inventory-page-oldest"];
            *h.view.inventory.lock().unwrap() = InventorySurvey {
                pages: pages.iter().map(|id| InventoryPageState {
                    reference: object(id, ObjectRole::InventoryPage),
                    operation_id: "finished-operation".into(),
                    all_objects_absent: true,
                }).collect(),
                objects: Vec::new(),
            };
            assert_eq!(h.run().await.unwrap().deleted_objects, 0);
            h.clock.advance(leases::UNREACHABLE_GRACE_MS);
            h.view.surveys.store(0, Ordering::SeqCst);
            // Since the survey, one page was replaced and another gained objects.
            let mut replaced = h.view.inventory.lock().unwrap().pages[0].clone();
            replaced.reference.ciphertext_sha256 = risunest_sync_wire::hash(b"replaced");
            let mut filled = h.view.inventory.lock().unwrap().pages[1].clone();
            filled.all_objects_absent = false;
            *h.view.confirmations.lock().unwrap() = vec![replaced, filled];
            let result = h.run().await.unwrap();
            assert_eq!(result.stop_reason, StopReason::Complete);
            assert_eq!(*h.view.retired_inventory.lock().unwrap(), ["inventory-page-oldest"]);
            assert_eq!(result.deleted_objects, 3);
            // Each aged page is confirmed by itself, not by surveying the inventory again.
            assert_eq!(h.view.surveys.load(Ordering::SeqCst), 2);
            // The two pages that changed wait seven days again.
            h.view.confirmations.lock().unwrap().clear();
            assert_eq!(h.run().await.unwrap().deleted_objects, 0);
            assert_eq!(h.view.retired_inventory.lock().unwrap().len(), 1);
        });
    }

    #[test]
    fn inventory_delete_refusals_release_the_marker_but_response_loss_keeps_it() {
        runtime().block_on(async {
            let h = Harness::new();
            h.view.jobs.lock().unwrap().references.push(h.view.known[0].clone());
            *h.view.inventory.lock().unwrap() = InventorySurvey {
                pages: vec![InventoryPageState {
                    reference: object("inventory-page-empty", ObjectRole::InventoryPage),
                    operation_id: "finished".into(),
                    all_objects_absent: true,
                }],
                objects: Vec::new(),
            };
            assert_eq!(h.run().await.unwrap().deleted_objects, 0);
            h.clock.advance(leases::UNREACHABLE_GRACE_MS);
            for kind in [ErrorKind::Unauthorized, ErrorKind::RateLimited, ErrorKind::Unsupported] {
                *h.view.inventory_delete_error.lock().unwrap() = Some(kind);
                assert_eq!(h.run().await.unwrap_err().kind, kind);
                assert!(h.provider.objects_with_role(ObjectRole::Lease).is_empty());
            }
            *h.view.inventory_delete_error.lock().unwrap() = Some(ErrorKind::Transient);
            assert_eq!(h.run().await.unwrap_err().kind, ErrorKind::Transient);
            assert!(h.payload_deletes().is_empty());
            assert_eq!(h.run().await.unwrap().stop_reason, StopReason::Lease);
            assert_eq!(h.view.retired_inventory.lock().unwrap().len(), 4);
        });
    }

    #[test]
    fn c_live_or_newly_protected_inventory_pages_do_not_retire() {
        runtime().block_on(async {
            let h = Harness::new();
            let page = object("inventory-page-protected", ObjectRole::InventoryPage);
            *h.view.inventory.lock().unwrap() = InventorySurvey {
                pages: vec![InventoryPageState {
                    reference: page,
                    operation_id: "resumed-operation".into(),
                    all_objects_absent: true,
                }],
                objects: Vec::new(),
            };
            assert_eq!(h.run().await.unwrap().deleted_objects, 0);
            h.clock.advance(leases::UNREACHABLE_GRACE_MS);
            h.view.protected.lock().unwrap().push("resumed-operation".into());
            assert_eq!(h.run().await.unwrap().deleted_objects, 2);
            assert!(h.view.retired_inventory.lock().unwrap().is_empty());

            h.view.protected.lock().unwrap().clear();
            assert_eq!(h.run().await.unwrap().deleted_objects, 0);
            assert!(h.view.retired_inventory.lock().unwrap().is_empty());
            h.view.inventory.lock().unwrap().pages[0].all_objects_absent = false;
            h.clock.advance(leases::UNREACHABLE_GRACE_MS);
            assert_eq!(h.run().await.unwrap().deleted_objects, 0);
            assert!(h.view.retired_inventory.lock().unwrap().is_empty());
        });
    }

    #[test]
    fn c_fresh_device_discovers_registered_payload_without_a_local_package_cache() {
        runtime().block_on(async {
            let upload_root = tempfile::tempdir().unwrap();
            let empty_cache = tempfile::tempdir().unwrap();
            let provider = Arc::new(FakeProvider::new(false));
            let repository = fake::repository();
            let descriptor = Descriptor::new("format-repository".into(), None).unwrap();
            let identity = JobIdentity {
                job_id: "interrupted-upload".into(),
                connection_id: "connection".into(),
                repository_id: repository.repository_id.clone(),
                capture_id: "capture".into(),
                capture: CaptureIdentity {
                    store_id: "store".into(),
                    library_epoch: "epoch".into(),
                    generation: "generation".into(),
                    selection_epoch: "selection".into(),
                    revision: 1,
                },
            };
            let mut journal = TransferJournal::open(upload_root.path(), identity.clone()).unwrap();
            let bytes = b"synthetic interrupted payload";
            let intent = ObjectIntent {
                repository_id: repository.repository_id.clone(),
                job_id: identity.job_id.clone(),
                object_id: "pack-interrupted".into(),
                role: ObjectRole::Pack,
                byte_length: bytes.len() as u64,
                sha256: risunest_sync_wire::hash(bytes),
            };
            let mut spool = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(journal.spool_path(&intent.object_id))
                .unwrap();
            spool.write_all(bytes).unwrap();
            spool.sync_all().unwrap();
            journal.register(&intent).unwrap();
            transfer_job::upload_registered(
                &mut journal,
                &intent.object_id,
                &descriptor.repository_id,
                &[7; 32],
                intent.byte_length,
                &intent.sha256,
                provider.as_ref(),
                &repository,
                &Cancellation::default(),
            )
            .await
            .unwrap();
            drop(journal);

            let test = fake::loopback_dependencies(fake::MemoryVault::default(), 1_000);
            let connected = ConnectedRepository {
                stored: super::super::connection_store::StoredConnection {
                    id: "connection".into(),
                    config: super::super::contract::ConnectionConfig {
                        provider: "synthetic".into(),
                        profile: None,
                        endpoint: "https://synthetic.invalid".into(),
                        account_id: "account".into(),
                        location: BTreeMap::new(),
                        oauth_profile: None,
                    },
                    descriptor,
                    descriptor_locator: super::super::contract::RemoteLocator {
                        connection_identity: repository.connection_identity.clone(),
                        collection: None,
                        object: "descriptor".into(),
                    },
                    provider_repository_id: repository.repository_id.clone(),
                    credential_ref: "credential".into(),
                    root_key_ref: "key".into(),
                    recovery_key_ref: "recovery-key".into(),
                    retention_policy: None, transfer_concurrency: None, labels: Default::default(),
                    capabilities: fake::capabilities(true),
                    created_at_ms: 1_000,
                    verified_at_ms: 1,
                    last_sync_at_ms: None,
                    last_backup_at_ms: None,
                },
                provider: provider.clone(),
                handle: repository,
                dependencies: test.dependencies,
                root_key: zeroize::Zeroizing::new([7; 32]),
            };
            let view = ConnectedRepositoryView {
                connected: &connected,
                writer_id: "other-device",
                policy: RetentionPolicy::DEFAULT,
                now_ms: NOW,
                unfinished: Vec::new(),
                cache_root: empty_cache.path(),
                reads: Default::default(),
            };

            // Both listings of the snapshots folder may hand out one cursor, but
            // never one object.
            let page = |object: &str, next: Option<&str>| Ok(super::super::contract::ObjectPage {
                objects: vec![ObjectReceipt {
                    locator: super::super::contract::RemoteLocator {
                        connection_identity: connected.handle.connection_identity.clone(),
                        collection: Some("snapshots".into()),
                        object: format!("snapshots/{object}"),
                    },
                    byte_length: 1, version: None, checksum: None, complete: true,
                }],
                next_cursor: next.map(Into::into),
            });
            for (collection, object) in [(Collection::Snapshots, "snapshot-a"), (Collection::Checkpoints, "checkpoint-a")] {
                provider.script_page(collection, page(object, Some("next")));
                provider.script_page(collection, Ok(super::super::contract::ObjectPage { objects: Vec::new(), next_cursor: None }));
            }
            let listed = view.snapshots(&Cancellation::default()).await.unwrap();
            assert_eq!(listed.iter().map(|object| object.locator.object.as_str()).collect::<Vec<_>>(),
                ["snapshots/snapshot-a", "snapshots/checkpoint-a"]);
            provider.script_page(Collection::Snapshots, page("snapshot-a", None));
            provider.script_page(Collection::Checkpoints, page("snapshot-a", None));
            assert_eq!(view.snapshots(&Cancellation::default()).await.unwrap_err().kind, ErrorKind::Corrupt);

            // Confirming one surveyed page reads that page and looks up its
            // objects, without listing the inventory again.
            let survey = view.read_inventory(&Cancellation::default()).await.unwrap();
            let (requests, listings) = (provider.request_count(), provider.listing_count());
            let current = view.inventory_page(&survey.pages[0].reference, &Cancellation::default()).await.unwrap().unwrap();
            assert_eq!((current.operation_id.as_str(), current.all_objects_absent), (identity.job_id.as_str(), false));
            assert_eq!((provider.request_count() - requests, provider.listing_count() - listings), (2, 0));
            let mut gone = survey.pages[0].reference.clone();
            gone.receipt.locator.object.push_str("-removed");
            assert!(view.inventory_page(&gone, &Cancellation::default()).await.unwrap().is_none());

            tokio::time::pause();
            provider.delay_metadata(Duration::from_secs(90));
            let survey = view.read_inventory(&Cancellation::default()).await.unwrap();
            assert_eq!(survey.objects.len(), 1);
            assert_eq!(survey.objects[0].object_id, intent.object_id);
            assert_eq!(survey.pages.len(), 1);
            assert!(!survey.pages[0].all_objects_absent);

            provider.delay_metadata(Duration::from_secs(181));
            let expired = view.read_inventory(&Cancellation::default()).await.err().unwrap();
            assert_eq!(expired.kind, ErrorKind::Transient);

            let cancel = Cancellation::default();
            let started = tokio::time::Instant::now();
            let (cancelled, ()) = tokio::join!(view.read_inventory(&cancel), async {
                tokio::time::sleep(Duration::from_secs(61)).await;
                cancel.cancel();
            });
            assert_eq!(cancelled.err().unwrap().kind, ErrorKind::Cancelled);
            assert!(started.elapsed() < Duration::from_secs(62));
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
    fn c_root_replacement_after_mark_prevents_deletion_but_keeps_age() {
        runtime().block_on(async {
            let h = Harness::new();
            h.age().await;
            h.view.change_at.store(h.view.reads.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
            assert_eq!(h.run().await.unwrap().stop_reason, StopReason::RootsChanged);
            assert!(h.payload_deletes().is_empty());
            h.view.change_at.store(usize::MAX, Ordering::SeqCst);
            assert_eq!(h.run().await.unwrap().deleted_objects, 2);
            assert_eq!(h.payload_deletes(), ["parent-catalog", "child-pack"]);
        });
    }
    #[test]
    fn c_hidden_or_yielded_runs_between_observations_keep_unreachable_age() {
        runtime().block_on(async {
            let h = Harness::new();
            h.age().await;
            h.clock.suspend();
            assert_eq!(h.run().await.unwrap().stop_reason, StopReason::Lease);
            h.clock.foreground();
            h.clock.set_trusted(true);
            assert_eq!(h.run().await.unwrap().deleted_objects, 2);

            let h = Harness::new();
            h.age().await;
            h.provider.seed("unknown-protection", ObjectRole::Lease, b"broken".to_vec());
            assert_eq!(h.run().await.unwrap().stop_reason, StopReason::Lease);
            h.provider.forget("unknown-protection");
            assert_eq!(h.run().await.unwrap().deleted_objects, 2);
            assert!(h.payload_deletes().len() == 2);
        });
    }
    #[test]
    fn c_a_newly_reachable_object_still_loses_its_age() {
        runtime().block_on(async {
            let h = Harness::new();
            h.age().await;
            h.view.jobs.lock().unwrap().references.push(h.view.known[1].clone());
            assert_eq!(h.run().await.unwrap().deleted_objects, 1);
            h.view.jobs.lock().unwrap().references.clear();
            assert_eq!(h.run().await.unwrap().deleted_objects, 0);
            assert_eq!(h.provider.delete_attempts("child-pack"), 0);
            h.clock.advance(leases::UNREACHABLE_GRACE_MS);
            assert_eq!(h.run().await.unwrap().deleted_objects, 1);
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
                assert_eq!(h.view.removed.lock().unwrap().len(), deleted as usize);
                assert_eq!(h.provider.delete_attempts("child-pack"), if deleted == 2 { 1 } else { 0 });
            }
        });
    }
    #[test]
    fn container_delete_refusals_release_the_marker_but_response_loss_keeps_it() {
        runtime().block_on(async {
            for kind in [ErrorKind::Unauthorized, ErrorKind::RateLimited, ErrorKind::Unsupported, ErrorKind::Transient] {
                let h = Harness::new();
                h.age().await;
                h.provider.state.lock().unwrap().container_delete_error = Some(kind);
                assert_eq!(h.run().await.unwrap_err().kind, kind);
                assert_eq!(h.payload_deletes(), ["parent-catalog", "child-pack"]);
                if kind == ErrorKind::Transient {
                    assert!(!h.provider.objects_with_role(ObjectRole::Lease).is_empty());
                    assert_eq!(h.run().await.unwrap().stop_reason, StopReason::Lease);
                } else {
                    assert!(h.provider.objects_with_role(ObjectRole::Lease).is_empty());
                    assert_eq!(h.run().await.unwrap().stop_reason, StopReason::Complete);
                }
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
            assert_eq!(h.provider.delete_attempts("child-pack"), 0);
            h.clock.advance(leases::LEASE_TTL_MS + leases::RELATIVE_CLOCK_ERROR_MS);
            // The parent is confirmed gone by probe, so its child keeps the age it had.
            assert_eq!(h.run().await.unwrap().deleted_objects, 1);
            assert_eq!(h.provider.delete_attempts("parent-catalog"), 1);
            assert_eq!(h.provider.delete_attempts("child-pack"), 1);
            h.clock.advance(leases::UNREACHABLE_GRACE_MS);
            assert_eq!(h.run().await.unwrap().deleted_objects, 0);
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
    /// A publication that stopped still holds the parent it selected, and the
    /// graph beneath that parent is what it is reusing.
    #[test]
    fn c_a_stopped_job_keeps_protecting_the_parent_it_recorded() {
        let directory = tempfile::tempdir().unwrap();
        let repository = fake::repository();
        let jobs = vec![UnfinishedJob {
            job_id: "job".into(), directory: directory.path().into(),
            snapshot_ids: BTreeSet::new(), references: Vec::new(),
        }];
        assert!(job_roots_of(&jobs, &repository).unwrap().references.is_empty());
        let parent = object("parent-catalog", ObjectRole::Catalog);
        std::fs::write(
            directory.path().join("parent.json"),
            serde_json::to_vec(&vec![parent.stored(&repository).unwrap()]).unwrap(),
        ).unwrap();
        assert_eq!(job_roots_of(&jobs, &repository).unwrap().references, vec![parent]);
        std::fs::write(directory.path().join("parent.json"), b"broken parent").unwrap();
        assert!(job_roots_of(&jobs, &repository).is_err());
    }

    /// A check stopped part way keeps the root it started on, even when no
    /// snapshot was named and the head it read has since moved on.
    #[test]
    fn c_a_stopped_check_keeps_protecting_the_root_it_pinned() {
        let directory = tempfile::tempdir().unwrap();
        let repository = fake::repository();
        let jobs = vec![UnfinishedJob {
            job_id: "check".into(), directory: directory.path().into(),
            snapshot_ids: BTreeSet::new(), references: Vec::new(),
        }];
        assert!(job_roots_of(&jobs, &repository).unwrap().references.is_empty());
        let checked = object("checked-state", ObjectRole::SyncState);
        super::super::repository_check::pin_root(directory.path(), &checked, &repository).unwrap();
        assert_eq!(job_roots_of(&jobs, &repository).unwrap().references, vec![checked]);
        std::fs::write(directory.path().join("checked-root.json"), b"broken root").unwrap();
        assert!(job_roots_of(&jobs, &repository).is_err(), "an unreadable pin became no root");
    }

    #[test]
    fn c_corrupt_job_journal_is_not_silently_dropped_from_protected_roots() {
        let directory = tempfile::tempdir().unwrap();
        let jobs = vec![UnfinishedJob {
            job_id: "job".into(), directory: directory.path().into(),
            snapshot_ids: BTreeSet::new(), references: Vec::new(),
        }];
        assert!(job_roots_of(&jobs, &fake::repository()).unwrap().objects.is_empty());
        std::fs::write(directory.path().join("transfers.sqlite"), b"broken journal").unwrap();
        assert!(job_roots_of(&jobs, &fake::repository()).is_err());
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
