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
