//! Bounded sync decision and publication orchestration.
//!
//! The decision is pure: callers obtain one authenticated remote head and one
//! short-lived PDS snapshot, then perform the selected network or activation
//! stage. No branch silently changes a repository's fixed publication strategy.
use super::{
    connection_commands::ConnectedRepository,
    contract::{Cancellation, ErrorKind, ProviderError, Result, VersionToken},
    control::{
        self, BackupPointDocument, HeadDocument, ObservedHead, PublicationResult,
    },
    job_store::{DurableJob, JobKind, JobStore},
    journal::{JobIdentity, TransferJournal},
    packaging::{CompletedSnapshot, PackageLimits, SnapshotMetadata, SnapshotPurpose},
    publication::HeadObservation,
};
use crate::persistent_store::{
    device_store::Section as PdsSection,
    external_apply::{ExternalSnapshotApplication, ExternalSnapshotObject, ExternalSnapshotRecord},
    external_conflicts::{ConflictPhase, ConflictPreservation, ConflictRecord},
    external_runtime::ExternalBase,
    sync_selection::CaptureIdentity,
};
use risunest_external_storage_format::{
    format::{library_fingerprint_domain, Descriptor},
    section::SectionKind,
};
use risunest_sync_wire::head::Sequence;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

struct ApplyClaim {
    app: AppHandle,
    job_id: String,
}
impl Drop for ApplyClaim {
    fn drop(&mut self) {
        if let Ok(mut active) = self
            .app
            .state::<super::job_store::JobCommandState>()
            .active
            .lock()
        {
            active.remove(&self.job_id);
        }
    }
}
fn claim_apply(app: &AppHandle, job: &DurableJob) -> Result<(Cancellation, ApplyClaim)> {
    let cancel = Cancellation::default();
    let state = app.state::<super::job_store::JobCommandState>();
    let mut active = state.active.lock().map_err(local_error)?;
    if active.contains_key(&job.id)
        || active
            .values()
            .any(|(connection, _)| connection == &job.request.connection_id)
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    active.insert(
        job.id.clone(),
        (job.request.connection_id.clone(), cancel.clone()),
    );
    Ok((
        cancel,
        ApplyClaim {
            app: app.clone(),
            job_id: job.id.clone(),
        },
    ))
}

fn corrupt(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredHeadObservation {
    commit_id: String,
    authenticated_body_hash: String,
    version: Option<VersionToken>,
}
impl From<&HeadObservation> for StoredHeadObservation {
    fn from(value: &HeadObservation) -> Self {
        Self {
            commit_id: value.commit_id.clone(),
            authenticated_body_hash: value.authenticated_body_hash.clone(),
            version: value.version.clone(),
        }
    }
}

pub(crate) fn observation_json(value: &HeadObservation) -> Result<String> {
    if value.commit_id.is_empty()
        || !crate::trust_boundary::is_lower_hex_256(&value.authenticated_body_hash)
        || value
            .version
            .as_ref()
            .is_some_and(|version| version.0.is_empty())
    {
        return Err(corrupt("invalid authenticated head observation"));
    }
    serde_json::to_string(&StoredHeadObservation::from(value)).map_err(corrupt)
}
fn parse_observation(value: &str) -> Result<StoredHeadObservation> {
    if value.is_empty() || value.len() > 16 * 1024 {
        return Err(corrupt("invalid stored head observation"));
    }
    let parsed: StoredHeadObservation = serde_json::from_str(value).map_err(corrupt)?;
    if parsed.commit_id.is_empty()
        || !crate::trust_boundary::is_lower_hex_256(&parsed.authenticated_body_hash)
        || parsed
            .version
            .as_ref()
            .is_some_and(|version| version.0.is_empty())
        || serde_json::to_string(&parsed).map_err(corrupt)? != value
    {
        return Err(corrupt("invalid stored head observation"));
    }
    Ok(parsed)
}

pub(crate) struct SyncInputs<'a> {
    pub descriptor: &'a Descriptor,
    pub connection_id: &'a str,
    pub current_identity: &'a CaptureIdentity,
    pub local_pristine: bool,
    /// Required once the local side is known to differ from its base.
    pub local_fingerprint: Option<&'a str>,
    /// A participating section holds a write this remote has not seen. Device
    /// values move without the library moving, so they need their own signal.
    pub local_sections_changed: bool,
    pub base: Option<&'a ExternalBase>,
    pub remote: Option<&'a ObservedHead>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SyncAction {
    UpToDate,
    PublishLocal {
        expected: Option<ObservedHead>,
    },
    ReceiveRemote {
        remote: ObservedHead,
    },
    /// The local contents already equal the new remote snapshot. Only the PDS
    /// base and current provider version need to advance.
    AcceptEquivalent {
        remote: ObservedHead,
    },
    PreserveConflict {
        remote: ObservedHead,
    },
    FirstAttachDecision {
        remote: ObservedHead,
    },
    DecisionRequired,
    RecoveryRequired,
}

/// The head this device agreed to, checked against the base row carrying it.
fn stored_observation(base: &ExternalBase) -> Result<StoredHeadObservation> {
    let stored = parse_observation(&base.head_observation)?;
    if stored.commit_id != base.commit_id {
        return Err(corrupt("stored base and head observation differ"));
    }
    Ok(stored)
}

/// Whether the remote carries other content than the base. A version token can
/// move without the content moving, so only the commit and the body it
/// authenticates count.
fn remote_content_differs(stored: &StoredHeadObservation, remote: &ObservedHead) -> bool {
    stored.commit_id != remote.observation.commit_id
        || stored.authenticated_body_hash != remote.observation.authenticated_body_hash
}

fn same_local_lineage(a: &CaptureIdentity, b: &CaptureIdentity) -> bool {
    a.store_id == b.store_id
        && a.library_epoch == b.library_epoch
        && a.generation == b.generation
        && a.selection_epoch == b.selection_epoch
}

pub(crate) fn decide_sync(input: SyncInputs<'_>) -> Result<SyncAction> {
    input.descriptor.validate().map_err(corrupt)?;
    if input.connection_id.is_empty() {
        return Err(corrupt("missing sync connection"));
    }
    if let Some(remote) = input.remote {
        if remote.document.repository_id != input.descriptor.repository_id {
            return Err(corrupt("remote head belongs to another repository"));
        }
    }
    let Some(base) = input.base else {
        return Ok(match input.remote {
            Some(remote) if input.local_pristine => SyncAction::ReceiveRemote {
                remote: remote.clone(),
            },
            Some(remote) => SyncAction::FirstAttachDecision {
                remote: remote.clone(),
            },
            None => SyncAction::PublishLocal { expected: None },
        });
    };
    if base.repository_id != input.descriptor.repository_id {
        return Ok(SyncAction::DecisionRequired);
    }
    if !same_local_lineage(input.current_identity, &base.identity)
        || input.current_identity.revision < base.identity.revision
    {
        return Ok(SyncAction::DecisionRequired);
    }
    let Some(remote) = input.remote else {
        return Ok(SyncAction::RecoveryRequired);
    };
    let stored = stored_observation(base)?;
    let local_changed = input.current_identity.revision != base.identity.revision;
    let remote_content_changed = remote_content_differs(&stored, remote);
    let remote_version_changed = stored.version != remote.observation.version;

    match (local_changed, remote_content_changed) {
        (false, false) if input.local_sections_changed => Ok(SyncAction::PublishLocal {
            expected: Some(remote.clone()),
        }),
        (false, false) if remote_version_changed => Ok(SyncAction::AcceptEquivalent {
            remote: remote.clone(),
        }),
        (false, false) => Ok(SyncAction::UpToDate),
        (false, true) => Ok(SyncAction::ReceiveRemote {
            remote: remote.clone(),
        }),
        (true, false) => Ok(SyncAction::PublishLocal {
            expected: Some(remote.clone()),
        }),
        (true, true) => {
            let local = input
                .local_fingerprint
                .ok_or_else(|| corrupt("local fingerprint was not captured"))?;
            if !crate::trust_boundary::is_lower_hex_256(local) {
                return Err(corrupt("invalid local fingerprint"));
            }
            if local == remote.document.content_fingerprint {
                Ok(SyncAction::AcceptEquivalent {
                    remote: remote.clone(),
                })
            } else {
                Ok(SyncAction::PreserveConflict {
                    remote: remote.clone(),
                })
            }
        }
    }
}

fn local_error(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

fn pds(app: &AppHandle) -> Result<crate::persistent_store::PersistentStore> {
    super::runtime::native_store(app)
}

async fn capture_for_publication(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    expected: Option<&ObservedHead>,
    cancel: &Cancellation,
) -> Result<(
    crate::persistent_store::external_capture::CapturedSnapshot,
    [u8; 32],
)> {
    let worker_app = app.clone();
    let worker_job = job.clone();
    let repository_id = connected.stored.descriptor.repository_id.clone();
    let strategy = connected
        .stored
        .descriptor
        .publication_strategy
        .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
    let expected_head = expected
        .map(|head| observation_json(&head.observation))
        .transpose()?;
    let cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        let probe = super::runtime::CancelProbe(cancel);
        let mut store = pds(&worker_app)?;
        if let Some(existing) = store
            .external_jobs(&worker_job.request.connection_id)
            .map_err(local_error)?
            .into_iter()
            .find(|item| item.id == worker_job.id)
        {
            if existing.phase != "ready" {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
            let admission = worker_app
                .state::<crate::native_file_jobs::NativeFileJobState>()
                .admission
                .clone();
            let _permit = admission.file(true).map_err(local_error)?;
            super::runtime::read_job_session(&worker_app, &worker_job.id)?;
            super::runtime::require_admitted_library(
                &worker_job,
                &store.external_identity().map_err(local_error)?,
            )?;
            let capture = store
                .reopen_external_capture(&existing.capture_id)
                .map_err(local_error)?;
            let fingerprint = capture
                .catalog
                .content_fingerprint(&library_fingerprint_domain())
                .map_err(local_error)?;
            return Ok((capture, fingerprint));
        }
        let hydration = store
            .hydrate_external_capture_dependencies(
                &worker_job.request.connection_id,
                &probe,
            )
            .map_err(local_error)?;
        let admission = worker_app
            .state::<crate::native_file_jobs::NativeFileJobState>()
            .admission
            .clone();
        let _permit = admission.file(true).map_err(local_error)?;
        let session = super::runtime::read_job_session(&worker_app, &worker_job.id)?;
        super::runtime::require_admitted_library(
            &worker_job,
            &store.external_identity().map_err(local_error)?,
        )?;
        let capture = store
            .capture_external_library(
                &worker_job.request.connection_id,
                &hydration,
                &probe,
            )
            .map_err(local_error)?;
        let intent = crate::persistent_store::external_storage_state::PublishIntent {
            job_id: &worker_job.id,
            connection_id: &worker_job.request.connection_id,
            repository_id: &repository_id,
            capture_id: &capture.id,
            identity: &capture.identity,
            strategy: match strategy {
                risunest_external_storage_format::format::Strategy::Cas => "cas",
                risunest_external_storage_format::format::Strategy::Sequential => "sequential",
            },
            expected_head: expected_head.as_deref(),
            commit_id: &worker_job.id,
        };
        if session == super::publication::ExecutionSession::ExitDrain {
            store
                .external_prepare_publication_exit_drain(&intent)
                .map_err(local_error)?;
        } else {
            store
                .external_prepare_publication(&intent)
                .map_err(local_error)?;
        }
        let jobs = JobStore::open(&super::runtime::root(&worker_app)?)?;
        let mut durable = jobs.read(&worker_job.id)?;
        durable.capture_id = Some(capture.id.clone());
        jobs.put(&durable)?;
        let fingerprint = capture
            .catalog
            .content_fingerprint(&library_fingerprint_domain())
            .map_err(local_error)?;
        Ok((capture, fingerprint))
    })
    .await
    .map_err(local_error)?
}

/// A publication produces a synchronized state. Conflict preservation produces
/// an immutable bundle instead, because the material it keeps is this device's
/// own and is never merged.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PackageAs {
    State,
    Bundle,
}

#[allow(clippy::too_many_arguments)]
async fn package_capture(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    capture: crate::persistent_store::external_capture::CapturedSnapshot,
    fingerprint: [u8; 32],
    expected: Option<&ObservedHead>,
    produce: PackageAs,
    cancel: &Cancellation,
) -> Result<(
    CompletedSnapshot,
    TransferJournal,
    Vec<super::sections::SectionPublication>,
)> {
    let root = super::runtime::root(app)?;
    let directory = super::runtime::job_directory(&root, &job.request.connection_id, &job.id);
    let identity = capture.identity.clone();
    let mut journal = TransferJournal::open(
        &directory,
        JobIdentity {
            job_id: job.id.clone(),
            connection_id: job.request.connection_id.clone(),
            repository_id: connected.handle.repository_id.clone(),
            capture_id: capture.id.clone(),
            capture: identity.clone(),
        },
    )?;
    // Step 4 of the publication contract needs the latest state, not just the
    // head: the new state inherits its sections and continues its commit order.
    let parent = match (produce, expected) {
        (PackageAs::State, Some(head)) => Some(
            control::read_snapshot_document(connected, &head.document.state, cancel).await?,
        ),
        _ => None,
    };
    // A section this device publishes takes the commit number the new state
    // gets. A bundle preserves this device's own material instead of joining a
    // commit order, so it carries none.
    let generation = match (produce, &parent) {
        (PackageAs::State, Some(view)) => Sequence::try_from(view.revision.clone())
            .map_err(corrupt)?
            .next()
            .map_err(corrupt)?,
        (PackageAs::State, None) => Sequence::from(1u64),
        (PackageAs::Bundle, _) => Sequence::from(0u64),
    };
    let (sections, publications) = {
        let worker_app = app.clone();
        let worker_spool = directory.join("sections");
        let worker_generation = generation.clone();
        let worker_cancel = cancel.clone();
        // The sections the observed state carries say which removals the remote
        // still holds and how far it has reclaimed, which is what decides both
        // whether this device may publish an increment and what it may drop.
        let worker_parent = parent
            .as_ref()
            .map(|view| view.sections.clone())
            .unwrap_or_default();
        let worker_connection = job.request.connection_id.clone();
        let worker_lineage = identity.library_epoch.clone();
        tokio::task::spawn_blocking(move || -> Result<_> {
            let mut store = pds(&worker_app)?;
            super::sections::capture_state_sections(
                &mut store,
                &worker_generation,
                &worker_parent,
                &worker_connection,
                &worker_lineage,
                &worker_spool,
                &worker_cancel,
            )
        })
        .await
        .map_err(local_error)??
    };
    let metadata = SnapshotMetadata {
        snapshot_id: job.snapshot_id.clone(),
        repository_id: connected.stored.descriptor.repository_id.clone(),
        library_id: identity.library_epoch.clone(),
        author_device_id: identity.store_id.clone(),
        created_at_ms: job.summary["startedAtMs"]
            .as_str()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| corrupt("invalid job timestamp"))?,
        logical_revision: u64::try_from(identity.revision).map_err(corrupt)?,
        parent_snapshot_id: parent.as_ref().map(|view| view.snapshot_id.clone()),
        content_fingerprint: fingerprint,
        purpose: match produce {
            PackageAs::State => SnapshotPurpose::SyncState {
                epoch: identity.generation.clone(),
                generation,
                parent_sections: parent.map(|view| view.sections).unwrap_or_default(),
            },
            PackageAs::Bundle => SnapshotPurpose::BackupBundle {
                source: risunest_external_storage_format::control::BundleSource::Device {
                    writer_id: identity.store_id.clone(),
                },
                remote_generation: None,
            },
        },
    };
    let cache = directory
        .parent()
        .and_then(|path| path.parent())
        .ok_or_else(|| corrupt("invalid external job directory"))?
        .join("package-cache");
    let completed = super::snapshot::package_and_upload(
        capture,
        sections,
        &root,
        &cache,
        metadata,
        &connected.root_key,
        PackageLimits::from_capabilities(&connected.stored.capabilities)?,
        &mut journal,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    Ok((completed, journal, publications))
}

async fn receive_remote(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    remote: &ObservedHead,
    cancel: &Cancellation,
) -> Result<Value> {
    let identity = pds(app)?.external_identity().map_err(local_error)?;
    let observation = observation_json(&remote.observation)?;
    let intent = crate::persistent_store::external_storage_state::ReceiveIntent {
        job_id: &job.id,
        connection_id: &job.request.connection_id,
        repository_id: &connected.stored.descriptor.repository_id,
        snapshot_id: remote
            .document
            .state
            .object_id
            .trim_start_matches("snapshot-"),
        commit_id: &remote.document.commit_id,
        authenticated_head: &observation,
        identity: &identity,
    };
    let existing = pds(app)?
        .external_jobs(&job.request.connection_id)
        .map_err(local_error)?
        .into_iter()
        .find(|item| item.id == job.id);
    if job.request.kind == JobKind::ResolveConflict {
        if existing
            .as_ref()
            .is_some_and(|item| matches!(item.phase.as_str(), "stale" | "cancelled"))
        {
            pds(app)?
                .external_prepare_conflict_receive(&intent)
                .map_err(local_error)?;
        }
    } else if existing.is_none() {
        pds(app)?
            .external_prepare_receive(&intent)
            .map_err(local_error)?;
    }
    let root = super::runtime::root(app)?;
    let staging =
        super::runtime::job_directory(&root, &job.request.connection_id, &job.id).join("receive");
    let prepared = super::snapshot_restore::download_snapshot(
        &remote.document.state,
        &staging,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    let current = control::read_head(
        connected.provider.as_ref(),
        &connected.handle,
        &connected.stored.descriptor,
        &connected.root_key,
        None,
        cancel,
    )
    .await?;
    if current.as_ref().map(|head| &head.observation) != Some(&remote.observation) {
        if job.request.kind != JobKind::ResolveConflict {
            pds(app)?
                .external_cancel_prepared(&job.id)
                .map_err(local_error)?;
        }
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(
        json!({"receiveReady":true,"snapshotId":prepared.snapshot_id,"expectedRevision":identity.revision.to_string()}),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApplyReceivedRequest {
    job_id: String,
    expected_revision: String,
}

/// The sections this device takes part in right now. A section left out is not
/// downloaded at all, so its values never reach this installation.
fn participating_sections(app: &AppHandle) -> Result<std::collections::BTreeSet<String>> {
    let mut store = pds(app)?;
    let device = store.device_store_mut().map_err(local_error)?;
    let mut wanted = std::collections::BTreeSet::new();
    for (kind, section) in [
        (SectionKind::Hypa, PdsSection::Hypa),
        (SectionKind::LocalPlugins, PdsSection::LocalPlugins),
    ] {
        if device.section_state(section).map_err(local_error)?.participating {
            wanted.insert(kind.id().to_owned());
        }
    }
    Ok(wanted)
}

/// Records what a confirmed publication put on the remote, so the next cycle
/// does not offer the same values again.
fn note_sections_published(
    app: &AppHandle,
    connection_id: &str,
    library_lineage: &str,
    sections: &std::collections::BTreeMap<
        String,
        risunest_external_storage_format::snapshot::SectionSnapshotRef,
    >,
    publications: &[super::sections::SectionPublication],
) -> Result<()> {
    if sections.is_empty() && publications.is_empty() {
        return Ok(());
    }
    let mut store = pds(app)?;
    let device = store.device_store_mut().map_err(local_error)?;
    for publication in publications {
        device
            .note_section_published(
                publication.section,
                &publication.published,
                &publication.stamped,
                &publication.first_published,
                &publication.reclaimed,
                &publication.gc_floor,
            )
            .map_err(local_error)?;
    }
    for reference in sections.values() {
        let Some(section) = super::sections::section_of(reference.kind) else {
            continue;
        };
        device
            .write_section_cursor(
                connection_id,
                library_lineage,
                section,
                &crate::persistent_store::device_store::sections::SectionCursor {
                    applied_generation: reference.generation.clone(),
                    applied_gc_floor: reference.gc_floor.clone(),
                    observed_max_write_clock: reference.max_write_clock.clone(),
                },
            )
            .map_err(local_error)?;
    }
    Ok(())
}

/// Merges received sections into the device file. Each section is its own
/// transaction, so an interrupted apply resumes from the same remote state
/// instead of reporting the whole receive as done.
fn apply_received_sections(
    app: &AppHandle,
    connection_id: &str,
    library_lineage: &str,
    sections: &[super::snapshot_restore::PreparedSection],
) -> Result<()> {
    if sections.is_empty() {
        return Ok(());
    }
    let mut store = pds(app)?;
    for prepared in sections {
        super::sections::apply_received_section(
            &mut store,
            connection_id,
            library_lineage,
            super::sections::SectionArrival::Continuing,
            prepared,
        )?;
    }
    Ok(())
}

/// Brings in the sections this remote lineage has never exchanged with this
/// device, before a publication can put local rows in their place. Without it
/// a device that takes a section back on publishes over whatever the other
/// devices left there.
async fn rejoin_sections(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    identity: &CaptureIdentity,
    base: &ExternalBase,
    remote: &ObservedHead,
    cancel: &Cancellation,
) -> Result<()> {
    if base.repository_id != connected.stored.descriptor.repository_id
        || !same_local_lineage(identity, &base.identity)
        || identity.revision < base.identity.revision
    {
        return Ok(());
    }
    let lineage = &base.identity.library_epoch;
    let (wanted, awaiting) = {
        let mut store = pds(app)?;
        let wanted =
            super::sections::rejoining_sections(&mut store, &job.request.connection_id, lineage)?;
        let awaiting = store
            .device_store_mut()
            .map_err(local_error)?
            .sections_await_publication(&job.request.connection_id, lineage)
            .map_err(local_error)?;
        (wanted, awaiting)
    };
    // Nothing takes their place while the library and the remote both stay
    // where they are, so the remote content is read only on a cycle that can
    // publish or receive.
    if wanted.is_empty()
        || !(awaiting
            || identity.revision != base.identity.revision
            || remote_content_differs(&stored_observation(base)?, remote))
    {
        return Ok(());
    }
    let staging = super::runtime::job_directory(
        &super::runtime::root(app)?,
        &job.request.connection_id,
        &job.id,
    )
    .join("rejoin");
    let received = super::snapshot_restore::download_sections(
        &remote.document.state,
        &wanted,
        &staging,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    let mut store = pds(app)?;
    for prepared in &received {
        super::sections::apply_received_section(
            &mut store,
            &job.request.connection_id,
            lineage,
            super::sections::SectionArrival::Rejoining,
            prepared,
        )?;
    }
    Ok(())
}

async fn apply_received(app: &AppHandle, request: &ApplyReceivedRequest) -> Result<Value> {
    if request.job_id.is_empty()
        || request.job_id.len() > 1024
        || request.expected_revision.parse::<i64>().is_err()
        || request.expected_revision.starts_with('-')
    {
        return Err(corrupt("invalid staged receive request"));
    }
    let root = super::runtime::root(app)?;
    let jobs = JobStore::open(&root)?;
    let mut job = jobs.read(&request.job_id)?;
    if job.summary["state"] != "waiting"
        || job.summary["phase"] != "remote-apply"
        || job.summary["result"]["receiveReady"] != true
        || job.summary["result"]["expectedRevision"] != request.expected_revision
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let (cancel, _claim) = claim_apply(app, &job)?;
    super::runtime::read_job_session(app, &job.id)?;
    let connected =
        super::connection_commands::open_connected(app, &job.request.connection_id).await?;
    let authoritative = pds(app)?
        .external_jobs(&job.request.connection_id)
        .map_err(local_error)?
        .into_iter()
        .find(|item| item.id == job.id && item.role == "restore" && item.phase == "ready")
        .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
    if authoritative.identity.revision.to_string() != request.expected_revision {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let remote = control::read_head(
        connected.provider.as_ref(),
        &connected.handle,
        &connected.stored.descriptor,
        &connected.root_key,
        None,
        &cancel,
    )
    .await?
    .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
    if observation_json(&remote.observation)?
        != authoritative
            .expected_head
            .as_deref()
            .ok_or_else(|| corrupt("missing staged head"))?
        || remote.document.commit_id != authoritative.commit_id
        || remote.document.state.object_id != format!("snapshot-{}", authoritative.capture_id)
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let staging =
        super::runtime::job_directory(&root, &job.request.connection_id, &job.id).join("receive");
    let prepared = super::snapshot_restore::download_snapshot(
        &remote.document.state,
        &staging,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        &cancel,
    )
    .await?;
    let wanted = participating_sections(app)?;
    let received_sections = super::snapshot_restore::download_sections(
        &remote.document.state,
        &wanted,
        &staging,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        &cancel,
    )
    .await?;
    let _permit = app
        .state::<crate::native_file_jobs::NativeFileJobState>()
        .admission
        .file(true)
        .map_err(local_error)?;
    super::runtime::read_job_session(app, &job.id)?;
    let latest = control::read_head(
        connected.provider.as_ref(),
        &connected.handle,
        &connected.stored.descriptor,
        &connected.root_key,
        None,
        &cancel,
    )
    .await?
    .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
    if latest.observation != remote.observation {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    cancel.check()?;
    super::runtime::read_job_session(app, &job.id)?;
    super::runtime::require_admitted_library(
        &job,
        &pds(app)?.external_identity().map_err(local_error)?,
    )?;
    let scope_id = library_fingerprint_domain();
    let fingerprint = hex::decode(&prepared.library_fingerprint)
        .ok()
        .and_then(|value| value.try_into().ok())
        .ok_or_else(|| corrupt("invalid received fingerprint"))?;
    let application = ExternalSnapshotApplication {
        expected_revision: authoritative.identity.revision,
        staging_root: &prepared.staging_root,
        scope_id: &scope_id,
        fingerprint: &fingerprint,
    };
    let records = prepared.records.into_iter().map(|record| {
        Ok(ExternalSnapshotRecord {
            key: record.key,
            content_hash: record.content_hash,
            byte_length: record.byte_length,
            path: record.path,
        })
    });
    let objects = prepared.objects.into_iter().map(|object| {
        Ok(ExternalSnapshotObject {
            content_hash: object.content_hash,
            byte_length: object.byte_length,
            path: object.path,
        })
    });
    // Sections go in before the library swap. A failure here leaves the job
    // ready, so the retry applies the same remote rows again and only then
    // replaces the library.
    apply_received_sections(
        app,
        &job.request.connection_id,
        &authoritative.identity.library_epoch,
        &received_sections,
    )?;
    let mut store = pds(app)?;
    let commit = store
        .prepare_external_snapshot_application(&application, records, objects)
        .map_err(local_error)?;
    let revision = store
        .finish_external_receive(commit, &job.id)
        .map_err(local_error)?
        .revision;
    if job.request.kind == JobKind::ResolveConflict {
        if store.external_finish_conflict(&job.id).is_err() {
            crate::nlog!(
                "error",
                "External receive committed but conflict bookkeeping did not finish"
            );
        }
    }
    let result = json!({"snapshotId":prepared.snapshot_id,"receivedRevision":revision.to_string()});
    job.summary["state"] = json!("succeeded");
    job.summary["phase"] = json!("complete");
    job.summary["result"] = result.clone();
    job.summary["updatedAtMs"] = json!(super::runtime::now_ms().to_string());
    if jobs.put(&job).is_err() {
        crate::nlog!(
            "error",
            "External receive committed but its UI summary could not be persisted"
        );
    }
    Ok(result)
}

#[tauri::command]
pub(crate) async fn external_storage_apply_received(
    app: AppHandle,
    request: ApplyReceivedRequest,
) -> Result<Value> {
    apply_received(&app, &request).await
}

async fn reconcile_unknown(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    cancel: &Cancellation,
) -> Result<Option<Value>> {
    let Some(intent) = pds(app)?
        .external_jobs(&job.request.connection_id)
        .map_err(local_error)?
        .into_iter()
        .find(|item| {
            item.id == job.id && matches!(item.phase.as_str(), "publishing" | "publicationUnknown")
        })
    else {
        return Ok(None);
    };
    let observed = control::read_head(
        connected.provider.as_ref(),
        &connected.handle,
        &connected.stored.descriptor,
        &connected.root_key,
        None,
        cancel,
    )
    .await?;
    let Some(observed) = observed else {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    };
    if observed.document.commit_id != intent.commit_id
        || observed.document.state.object_id != format!("snapshot-{}", job.snapshot_id)
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    if intent.phase == "publishing" {
        pds(app)?
            .external_publication_unknown(&job.id)
            .map_err(local_error)?;
    }
    let observation = observation_json(&observed.observation)?;
    let session = super::runtime::read_job_session(app, &job.id)?;
    if session == super::publication::ExecutionSession::ExitDrain {
        pds(app)?
            .external_confirm_publication_exit_drain(
                &job.id,
                &intent.commit_id,
                &job.snapshot_id,
                &observation,
                false,
            )
            .map_err(local_error)?;
    } else {
        pds(app)?
            .external_confirm_publication(
                &job.id,
                &intent.commit_id,
                &job.snapshot_id,
                &observation,
                false,
            )
            .map_err(local_error)?;
    }
    Ok(Some(
        json!({"snapshotId":job.snapshot_id,"publishedRevision":intent.identity.revision.to_string()}),
    ))
}

fn encode_remote(value: &super::packaging::RemoteObject) -> Result<String> {
    serde_json::to_string(value).map_err(corrupt)
}
fn decode_remote(
    value: &str,
    connected: &ConnectedRepository,
) -> Result<super::packaging::RemoteObject> {
    if value.is_empty() || value.len() > 256 * 1024 {
        return Err(corrupt("invalid preserved snapshot"));
    }
    let object: super::packaging::RemoteObject = serde_json::from_str(value).map_err(corrupt)?;
    if serde_json::to_string(&object).map_err(corrupt)? != value
        || object.repository_id != connected.stored.descriptor.repository_id
        || !matches!(
            object.role,
            super::contract::ObjectRole::SyncState | super::contract::ObjectRole::BackupBundle
        )
    {
        return Err(corrupt("preserved snapshot binding differs"));
    }
    object.stored(&connected.handle)?;
    Ok(object)
}

async fn preserve_conflict(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    remote: ObservedHead,
    capture: crate::persistent_store::external_capture::CapturedSnapshot,
    fingerprint: [u8; 32],
    cancel: &Cancellation,
) -> Result<Value> {
    let local_identity = capture.identity.clone();
    let local_capture_id = capture.id.clone();
    let record = record_local_conflict(
        app,
        connected,
        job,
        None,
        &local_capture_id,
        &local_identity,
        Some(&remote),
        false,
    )?;
    // A bundle keeps this device's own material instead of joining the
    // synchronized commit order, so it publishes nothing to mark.
    let (local, journal, _) = package_capture(
        app,
        connected,
        job,
        capture,
        fingerprint,
        None,
        PackageAs::Bundle,
        cancel,
    )
    .await?;
    let record = bind_local_conflict_snapshot(app, job, record, &local.reference)?;
    complete_conflict_preservation(app, connected, job, record, Some(remote), journal, cancel)
        .await
}

fn record_local_conflict(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    local: Option<&super::packaging::RemoteObject>,
    local_capture_id: &str,
    local_identity: &CaptureIdentity,
    remote: Option<&ObservedHead>,
    after_rejection: bool,
) -> Result<ConflictRecord> {
    let record = ConflictRecord {
        id: job.id.clone(),
        connection_id: job.request.connection_id.clone(),
        repository_id: connected.stored.descriptor.repository_id.clone(),
        local_capture_id: local_capture_id.into(),
        local_snapshot: local.map(encode_remote).transpose()?,
        local_identity: local_identity.clone(),
        remote_snapshot: remote
            .map(|head| encode_remote(&head.document.state))
            .transpose()?,
        remote_logical_revision: None,
        remote_commit_id: remote.map(|head| head.document.commit_id.clone()),
        remote_head_observation: remote
            .map(|head| observation_json(&head.observation))
            .transpose()?,
        created_at_ms: i64::try_from(super::runtime::now_ms()).map_err(corrupt)?,
        preservation: ConflictPreservation::LocalOnly,
        phase: ConflictPhase::Pending,
    };
    if after_rejection {
        pds(app)?
            .external_record_local_conflict_after_rejection(&record)
            .map_err(local_error)?;
    } else if super::runtime::read_job_session(app, &job.id)?
        == super::publication::ExecutionSession::ExitDrain
    {
        pds(app)?
            .external_record_local_conflict_exit_drain(&record)
            .map_err(local_error)?;
    } else {
        pds(app)?
            .external_record_local_conflict(&record)
            .map_err(local_error)?;
    }
    Ok(record)
}

fn bind_local_conflict_snapshot(
    app: &AppHandle,
    job: &DurableJob,
    record: ConflictRecord,
    local: &super::packaging::RemoteObject,
) -> Result<ConflictRecord> {
    let encoded = encode_remote(local)?;
    let session = super::runtime::read_job_session(app, &job.id)?;
    if session == super::publication::ExecutionSession::ExitDrain {
        pds(app)?
            .external_bind_local_conflict_snapshot_exit_drain(&record.id, &encoded)
            .map_err(local_error)
    } else {
        pds(app)?
            .external_bind_local_conflict_snapshot(&record.id, &encoded)
            .map_err(local_error)
    }
}

fn conflict_journal(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    record: &ConflictRecord,
) -> Result<TransferJournal> {
    let root = super::runtime::root(app)?;
    let directory = super::runtime::job_directory(&root, &job.request.connection_id, &job.id);
    TransferJournal::open(
        &directory,
        JobIdentity {
            job_id: job.id.clone(),
            connection_id: job.request.connection_id.clone(),
            repository_id: connected.handle.repository_id.clone(),
            capture_id: record.local_capture_id.clone(),
            capture: record.local_identity.clone(),
        },
    )
}

async fn resume_conflict_preservation(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    record: ConflictRecord,
    cancel: &Cancellation,
) -> Result<Value> {
    if record.local_snapshot.is_some() {
        let journal = conflict_journal(app, connected, job, &record)?;
        return complete_conflict_preservation(
            app, connected, job, record, None, journal, cancel,
        )
        .await;
    }
    let capture = pds(app)?
        .reopen_external_capture(&record.local_capture_id)
        .map_err(local_error)?;
    if capture.identity != record.local_identity {
        return Err(corrupt("conflict capture identity differs"));
    }
    let fingerprint = capture
        .catalog
        .content_fingerprint(&library_fingerprint_domain())
        .map_err(local_error)?;
    // A bundle keeps this device's own material instead of joining the
    // synchronized commit order, so it publishes nothing to mark.
    let (local, journal, _) = package_capture(
        app,
        connected,
        job,
        capture,
        fingerprint,
        None,
        PackageAs::Bundle,
        cancel,
    )
    .await?;
    let record = bind_local_conflict_snapshot(app, job, record, &local.reference)?;
    complete_conflict_preservation(app, connected, job, record, None, journal, cancel).await
}

async fn complete_conflict_preservation(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    record: ConflictRecord,
    observed_remote: Option<ObservedHead>,
    mut journal: TransferJournal,
    cancel: &Cancellation,
) -> Result<Value> {
    if record.preservation != ConflictPreservation::LocalOnly
        || record.connection_id != job.request.connection_id
        || record.repository_id != connected.stored.descriptor.repository_id
    {
        return Err(corrupt("invalid local conflict preservation"));
    }
    let local_snapshot = record
        .local_snapshot
        .as_deref()
        .ok_or_else(|| corrupt("local conflict upload is incomplete"))?;
    let (remote_snapshot, remote_commit_id, remote_observation) = match (
        record.remote_snapshot.as_deref(),
        record.remote_commit_id.as_deref(),
        record.remote_head_observation.as_deref(),
    ) {
        (Some(snapshot), Some(commit), Some(observation)) => (
            decode_remote(snapshot, connected)?,
            commit.to_owned(),
            observation.to_owned(),
        ),
        (None, None, None) => {
            let remote = match observed_remote {
                Some(remote) => remote,
                None => control::read_head(
                    connected.provider.as_ref(),
                    &connected.handle,
                    &connected.stored.descriptor,
                    &connected.root_key,
                    None,
                    cancel,
                )
                .await?
                .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?,
            };
            (
                remote.document.state.clone(),
                remote.document.commit_id.clone(),
                observation_json(&remote.observation)?,
            )
        }
        _ => return Err(corrupt("partial remote conflict binding")),
    };
    let root = super::runtime::root(app)?;
    let staging = super::runtime::job_directory(&root, &job.request.connection_id, &job.id)
        .join("conflict-remote");
    let verified_remote = super::snapshot_restore::download_snapshot(
        &remote_snapshot,
        &staging,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    if verified_remote.snapshot_id
        != remote_snapshot.object_id.trim_start_matches("snapshot-")
    {
        return Err(corrupt("conflict snapshot identity differs"));
    }
    let created_at_ms = u64::try_from(record.created_at_ms).map_err(corrupt)?;
    let remote_bundle = if remote_snapshot.role == super::contract::ObjectRole::BackupBundle {
        remote_snapshot.clone()
    } else {
        let view = control::read_snapshot_document(connected, &remote_snapshot, cancel).await?;
        control::upload_backup_bundle(
            &connected.stored.descriptor,
            &connected.root_key,
            format!("conflict-{}-remote", job.id),
            risunest_external_storage_format::control::BundleSource::SyncState {
                commit_id: remote_commit_id.clone(),
            },
            created_at_ms,
            view.library,
            view.sections,
            &mut journal,
            connected.provider.as_ref(),
            &connected.handle,
            cancel,
        )
        .await?
    };
    let point = BackupPointDocument::conflict(
        &connected.stored.descriptor,
        format!("conflict-{}", job.id),
        created_at_ms,
        decode_remote(local_snapshot, connected)?,
        remote_bundle,
    )?;
    control::upload_backup_point(
        &connected.stored.descriptor,
        &connected.root_key,
        point,
        &mut journal,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    let encoded_remote = encode_remote(&remote_snapshot)?;
    let remote_revision = i64::try_from(verified_remote.logical_revision).map_err(corrupt)?;
    let session = super::runtime::read_job_session(app, &job.id)?;
    if session == super::publication::ExecutionSession::ExitDrain {
        pds(app)?
            .external_complete_conflict_preservation_exit_drain(
                &record.id,
                &encoded_remote,
                remote_revision,
                &remote_commit_id,
                &remote_observation,
            )
            .map_err(local_error)?;
    } else {
        pds(app)?
            .external_complete_conflict_preservation(
                &record.id,
                &encoded_remote,
                remote_revision,
                &remote_commit_id,
                &remote_observation,
            )
            .map_err(local_error)?;
    }
    let _ = journal
        .release_completed_sessions(connected.dependencies.vault.as_ref())
        .await;
    Ok(json!({"snapshotId":decode_remote(local_snapshot, connected)?.object_id.trim_start_matches("snapshot-"),"conflictId":job.id,"preservation":"remote-complete"}))
}

async fn run_resolve_conflict(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    cancel: &Cancellation,
) -> Result<Value> {
    let conflict_id = job
        .request
        .conflict_id
        .as_deref()
        .ok_or_else(|| corrupt("missing conflict selection"))?;
    if conflict_id != job.id {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let choice = job
        .request
        .choice
        .as_deref()
        .ok_or_else(|| corrupt("missing conflict choice"))?;
    let stored = pds(app)?
        .external_conflict(conflict_id)
        .map_err(local_error)?
        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
    if stored.connection_id != job.request.connection_id
        || stored.repository_id != connected.stored.descriptor.repository_id
    {
        return Err(corrupt("conflict connection binding differs"));
    }
    let remote = control::read_head(
        connected.provider.as_ref(),
        &connected.handle,
        &connected.stored.descriptor,
        &connected.root_key,
        None,
        cancel,
    )
    .await?
    .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
    let observation = observation_json(&remote.observation)?;
    if stored.preservation != ConflictPreservation::RemoteComplete
        || stored.remote_head_observation.as_deref() != Some(observation.as_str())
        || stored.remote_commit_id.as_deref() != Some(remote.document.commit_id.as_str())
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    if choice == "remote" {
        pds(app)?
            .external_begin_conflict_resolution(conflict_id, &observation)
            .map_err(local_error)?;
        return match receive_remote(app, connected, job, &remote, cancel).await {
            Ok(result) => Ok(result),
            Err(error) => {
                if pds(app)?
                    .external_reject_conflict_resolution(conflict_id)
                    .is_err()
                {
                    crate::nlog!("error", "Failed to reset a rejected remote conflict choice");
                }
                Err(error)
            }
        };
    }
    if choice != "local" {
        return Err(corrupt("invalid conflict choice"));
    }
    let local = decode_remote(
        stored
            .local_snapshot
            .as_deref()
            .ok_or_else(|| corrupt("local conflict upload is incomplete"))?,
        connected,
    )?;
    let local_document = control::read_snapshot_document(connected, &local, cancel).await?;
    let strategy = connected
        .stored
        .descriptor
        .publication_strategy
        .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
    let commit_id = format!("resolve-{}", job.id);
    // The preserved material is a bundle. A head points at a state, so the
    // resolution republishes that library reference as one and continues the
    // remote commit order from the head it is replacing.
    let remote_state = control::read_snapshot_document(connected, &remote.document.state, cancel)
        .await?;
    let mut resolve_journal = TransferJournal::open(
        &super::runtime::job_directory(
            &super::runtime::root(app)?,
            &job.request.connection_id,
            &job.id,
        ),
        JobIdentity {
            job_id: job.id.clone(),
            connection_id: job.request.connection_id.clone(),
            repository_id: connected.handle.repository_id.clone(),
            capture_id: stored.local_capture_id.clone(),
            capture: stored.local_identity.clone(),
        },
    )?;
    let (state, state_fingerprint) = control::upload_sync_state(
        &connected.stored.descriptor,
        &connected.root_key,
        commit_id.clone(),
        remote.document.library_id.clone(),
        stored.local_identity.generation.clone(),
        Sequence::try_from(remote_state.revision.clone())
            .map_err(corrupt)?
            .next()
            .map_err(corrupt)?,
        Some(remote_state.snapshot_id.clone()),
        stored.local_identity.store_id.clone(),
        u64::try_from(stored.created_at_ms).map_err(corrupt)?,
        local_document.library.clone(),
        remote_state.sections.clone(),
        &mut resolve_journal,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    let document = HeadDocument::new(
        &connected.stored.descriptor,
        remote.document.library_id.clone(),
        commit_id.clone(),
        Some(remote.document.commit_id.clone()),
        state_fingerprint,
        state,
    )?;
    let prepared = control::prepare_head(
        &connected.stored.descriptor,
        &connected.root_key,
        &connected.handle,
        document,
    )?;
    pds(app)?
        .external_begin_conflict_resolution(conflict_id, &observation)
        .map_err(local_error)?;
    let intent = crate::persistent_store::external_storage_state::PublishIntent {
        job_id: &job.id,
        connection_id: &job.request.connection_id,
        repository_id: &connected.stored.descriptor.repository_id,
        capture_id: &stored.local_capture_id,
        identity: &stored.local_identity,
        strategy: match strategy {
            risunest_external_storage_format::format::Strategy::Cas => "cas",
            risunest_external_storage_format::format::Strategy::Sequential => "sequential",
        },
        expected_head: Some(&observation),
        commit_id: &commit_id,
    };
    if let Err(error) = pds(app)?.external_prepare_conflict_publication(&intent) {
        let _ = pds(app)?.external_reject_conflict_resolution(conflict_id);
        return Err(local_error(error));
    }
    let _permit = app
        .state::<crate::native_file_jobs::NativeFileJobState>()
        .admission
        .file(false)
        .map_err(local_error)?;
    let write_session = std::sync::atomic::AtomicU8::new(0);
    let result = match control::publish_head_guarded(
        connected.provider.as_ref(),
        &connected.handle,
        &connected.stored.capabilities,
        &connected.stored.descriptor,
        &connected.root_key,
        strategy,
        Some(&remote),
        &prepared,
        || super::runtime::read_job_session(app, &job.id),
        |live| {
            if live == super::publication::ExecutionSession::ExitDrain {
                pds(app)?
                    .external_begin_publication_exit_drain(&job.id)
                    .map_err(local_error)?;
                write_session.store(1, std::sync::atomic::Ordering::Release);
            } else {
                pds(app)?
                    .external_begin_publication(&job.id)
                    .map_err(local_error)?;
                write_session.store(2, std::sync::atomic::Ordering::Release);
            }
            Ok(())
        },
        cancel,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            pds(app)?
                .external_reject_conflict_resolution(conflict_id)
                .map_err(local_error)?;
            return Err(error);
        }
    };
    match result {
        PublicationResult::Confirmed(head) => {
            let observation = observation_json(&head.observation)?;
            if write_session.load(std::sync::atomic::Ordering::Acquire) == 1 {
                pds(app)?
                    .external_confirm_publication_exit_drain(
                        &job.id,
                        &commit_id,
                        &local_document.snapshot_id,
                        &observation,
                        false,
                    )
                    .map_err(local_error)?;
            } else {
                pds(app)?
                    .external_confirm_publication(
                        &job.id,
                        &commit_id,
                        &local_document.snapshot_id,
                        &observation,
                        false,
                    )
                    .map_err(local_error)?;
            }
            if pds(app)?.external_finish_conflict(conflict_id).is_err() {
                crate::nlog!("error","External conflict publication committed but conflict bookkeeping did not finish");
            }
            Ok(
                json!({"snapshotId":local_document.snapshot_id,"publishedRevision":stored.local_identity.revision.to_string()}),
            )
        }
        PublicationResult::Conflict(_) => {
            pds(app)?
                .external_publication_rejected(&job.id)
                .map_err(local_error)?;
            pds(app)?
                .external_reject_conflict_resolution(conflict_id)
                .map_err(local_error)?;
            Err(ProviderError::new(ErrorKind::PreconditionFailed))
        }
        PublicationResult::Unknown { .. } => {
            pds(app)?
                .external_publication_unknown(&job.id)
                .map_err(local_error)?;
            pds(app)?
                .external_conflict_publication_unknown(conflict_id)
                .map_err(local_error)?;
            Err(ProviderError::new(ErrorKind::Transient))
        }
    }
}

pub(crate) async fn run_sync(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    cancel: &Cancellation,
) -> Result<Value> {
    if job.request.kind == JobKind::ResolveConflict {
        if let Some(result) = reconcile_unknown(app, connected, job, cancel).await? {
            if pds(app)?.external_finish_conflict(&job.id).is_err() {
                crate::nlog!("error","External conflict publication reconciled but conflict bookkeeping did not finish");
            }
            return Ok(result);
        }
        return run_resolve_conflict(app, connected, job, cancel).await;
    }
    if let Some(result) = reconcile_unknown(app, connected, job, cancel).await? {
        return Ok(result);
    }
    let session = super::runtime::read_job_session(app, &job.id)?;
    let strategy = connected
        .stored
        .descriptor
        .publication_strategy
        .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
    connected.stored.capabilities.require(strategy)?;
    if strategy == risunest_external_storage_format::format::Strategy::Sequential
        && session == super::publication::ExecutionSession::Hidden
    {
        return Err(ProviderError::new(ErrorKind::Cancelled));
    }
    if let Some(record) = pds(app)?
        .external_conflict(&job.id)
        .map_err(local_error)?
        .filter(|record| record.preservation == ConflictPreservation::LocalOnly)
    {
        return resume_conflict_preservation(app, connected, job, record, cancel).await;
    }
    let remote = control::read_head(
        connected.provider.as_ref(),
        &connected.handle,
        &connected.stored.descriptor,
        &connected.root_key,
        None,
        cancel,
    )
    .await?;
    let store = pds(app)?;
    let identity = store.external_identity().map_err(local_error)?;
    let local_pristine = store.external_library_is_pristine().map_err(local_error)?;
    let base = store
        .external_base(&job.request.connection_id)
        .map_err(local_error)?;
    drop(store);
    // A section this lineage has not exchanged with yet is merged before the
    // decision, so neither a publication nor a receive settles it against a
    // local copy that never saw the remote rows.
    if let (Some(base), Some(remote)) = (base.as_ref(), remote.as_ref()) {
        rejoin_sections(app, connected, job, &identity, base, remote, cancel).await?;
    }
    let local_changed = base.as_ref().is_none_or(|base| {
        identity.revision != base.identity.revision
            || !same_local_lineage(&identity, &base.identity)
    });
    let sections_changed = match (&base, &remote) {
        (Some(base), Some(_)) => {
            let mut store = pds(app)?;
            let device = store.device_store_mut().map_err(local_error)?;
            device
                .sections_await_publication(
                    &job.request.connection_id,
                    &base.identity.library_epoch,
                )
                .map_err(local_error)?
        }
        _ => false,
    };
    let mut captured = None;
    let mut fingerprint = None;
    if (local_changed || sections_changed) && !(base.is_none() && remote.is_some() && local_pristine)
    {
        let value = capture_for_publication(app, connected, job, remote.as_ref(), cancel).await?;
        fingerprint = Some(hex::encode(value.1));
        captured = Some(value);
    }
    // A resumed job may retain an R5 capture while the live library has
    // already advanced to R10. The retained capture is the only identity that
    // can be bound to this publication and its resulting base.
    let decision_identity = captured
        .as_ref()
        .map(|(capture, _)| &capture.identity)
        .unwrap_or(&identity);
    let decision_revision = decision_identity.revision;
    let action = decide_sync(SyncInputs {
        descriptor: &connected.stored.descriptor,
        connection_id: &job.request.connection_id,
        current_identity: decision_identity,
        local_pristine,
        local_fingerprint: fingerprint.as_deref(),
        local_sections_changed: sections_changed,
        base: base.as_ref(),
        remote: remote.as_ref(),
    })?;
    match action {
        SyncAction::UpToDate => {
            if captured.is_some() {
                pds(app)?
                    .external_cancel_prepared(&job.id)
                    .map_err(local_error)?;
            }
            Ok(json!({"publishedRevision":decision_revision.to_string()}))
        }
        SyncAction::ReceiveRemote { remote } => {
            receive_remote(app, connected, job, &remote, cancel).await
        }
        SyncAction::AcceptEquivalent { remote } => {
            let accepted_identity = captured
                .as_ref()
                .map(|(capture, _)| capture.identity.clone())
                .unwrap_or_else(|| identity.clone());
            if captured.is_some() {
                pds(app)?
                    .external_cancel_prepared(&job.id)
                    .map_err(local_error)?;
            }
            let base = base.ok_or_else(|| corrupt("equivalent state has no base"))?;
            let observation = observation_json(&remote.observation)?;
            if session == super::publication::ExecutionSession::ExitDrain {
                pds(app)?
                    .external_accept_equivalent_exit_drain(
                        &job.request.connection_id,
                        &connected.stored.descriptor.repository_id,
                        &base.commit_id,
                        &base.head_observation,
                        remote
                            .document
                            .state
                            .object_id
                            .trim_start_matches("snapshot-"),
                        &remote.document.commit_id,
                        &observation,
                        &accepted_identity,
                    )
                    .map_err(local_error)?;
            } else {
                pds(app)?
                    .external_accept_equivalent(
                        &job.request.connection_id,
                        &connected.stored.descriptor.repository_id,
                        &base.commit_id,
                        &base.head_observation,
                        remote
                            .document
                            .state
                            .object_id
                            .trim_start_matches("snapshot-"),
                        &remote.document.commit_id,
                        &observation,
                        &accepted_identity,
                    )
                    .map_err(local_error)?;
            }
            Ok(
                json!({"snapshotId":remote.document.state.object_id.trim_start_matches("snapshot-"),"publishedRevision":accepted_identity.revision.to_string()}),
            )
        }
        SyncAction::PublishLocal { expected } => {
            let (capture, fingerprint) = captured
                .take()
                .ok_or_else(|| corrupt("missing publication capture"))?;
            let published_identity = capture.identity.clone();
            let local_capture_id = capture.id.clone();
            let (completed, mut journal, publications) = package_capture(
                app,
                connected,
                job,
                capture,
                fingerprint,
                expected.as_ref(),
                PackageAs::State,
                cancel,
            )
            .await?;
            let document = HeadDocument::new(
                &connected.stored.descriptor,
                expected
                    .as_ref()
                    .map(|head| head.document.library_id.clone())
                    .unwrap_or_else(|| published_identity.library_epoch.clone()),
                job.id.clone(),
                expected
                    .as_ref()
                    .map(|head| head.document.commit_id.clone()),
                completed.fingerprint.clone(),
                completed.reference.clone(),
            )?;
            let prepared = control::prepare_head(
                &connected.stored.descriptor,
                &connected.root_key,
                &connected.handle,
                document,
            )?;
            let _permit = app
                .state::<crate::native_file_jobs::NativeFileJobState>()
                .admission
                .file(false)
                .map_err(local_error)?;
            let write_session = std::sync::atomic::AtomicU8::new(0);
            match control::publish_head_guarded(
                connected.provider.as_ref(),
                &connected.handle,
                &connected.stored.capabilities,
                &connected.stored.descriptor,
                &connected.root_key,
                strategy,
                expected.as_ref(),
                &prepared,
                || super::runtime::read_job_session(app, &job.id),
                |live| {
                    if live == super::publication::ExecutionSession::ExitDrain {
                        pds(app)?
                            .external_begin_publication_exit_drain(&job.id)
                            .map_err(local_error)?;
                        write_session.store(1, std::sync::atomic::Ordering::Release);
                    } else {
                        pds(app)?
                            .external_begin_publication(&job.id)
                            .map_err(local_error)?;
                        write_session.store(2, std::sync::atomic::Ordering::Release);
                    }
                    Ok(())
                },
                cancel,
            )
            .await?
            {
                PublicationResult::Confirmed(observed) => {
                    let observation = observation_json(&observed.observation)?;
                    let session = write_session.load(std::sync::atomic::Ordering::Acquire);
                    if session == 0 {
                        return Err(corrupt("missing publication session"));
                    }
                    if session == 1 {
                        pds(app)?
                            .external_confirm_publication_exit_drain(
                                &job.id,
                                &job.id,
                                &completed.snapshot_id,
                                &observation,
                                false,
                            )
                            .map_err(local_error)?;
                    } else {
                        pds(app)?
                            .external_confirm_publication(
                                &job.id,
                                &job.id,
                                &completed.snapshot_id,
                                &observation,
                                false,
                            )
                            .map_err(local_error)?;
                    }
                    let _ = journal
                        .release_completed_sessions(connected.dependencies.vault.as_ref())
                        .await;
                    note_sections_published(
                        app,
                        &job.request.connection_id,
                        &published_identity.library_epoch,
                        &completed.sections,
                        &publications,
                    )?;
                    Ok(
                        json!({"snapshotId":completed.snapshot_id,"publishedRevision":published_identity.revision.to_string()}),
                    )
                }
                PublicationResult::Conflict(remote) => {
                    drop(_permit);
                    let record = record_local_conflict(
                        app,
                        connected,
                        job,
                        Some(&completed.reference),
                        &local_capture_id,
                        &published_identity,
                        remote.as_ref(),
                        true,
                    )?;
                    complete_conflict_preservation(
                        app, connected, job, record, remote, journal, cancel,
                    )
                    .await
                }
                PublicationResult::Unknown { .. } => {
                    pds(app)?
                        .external_publication_unknown(&job.id)
                        .map_err(local_error)?;
                    Err(ProviderError::new(ErrorKind::Transient))
                }
            }
        }
        SyncAction::PreserveConflict { remote } | SyncAction::FirstAttachDecision { remote } => {
            let (capture, fingerprint) = captured
                .take()
                .ok_or_else(|| corrupt("missing conflict capture"))?;
            preserve_conflict(app, connected, job, remote, capture, fingerprint, cancel).await
        }
        SyncAction::DecisionRequired | SyncAction::RecoveryRequired => {
            Err(ProviderError::new(ErrorKind::PreconditionFailed))
        }
    }
}

#[tauri::command]
pub(crate) fn external_storage_list_conflicts(
    app: AppHandle,
    connection_id: String,
) -> Result<Vec<Value>> {
    if connection_id.is_empty() || connection_id.len() > 1024 {
        return Err(corrupt("invalid connection"));
    }
    pds(&app)?.external_conflicts(&connection_id).map_err(local_error)?.into_iter().map(|record|Ok(json!({
        "id":record.id,"connectionId":record.connection_id,"detectedAtMs":record.created_at_ms.to_string(),
        "localRevision":record.local_identity.revision.to_string(),"remoteRevision":record.remote_logical_revision.map(|value|value.to_string()),
        "preservation":match record.preservation { ConflictPreservation::LocalOnly=>"local-only", ConflictPreservation::RemoteComplete=>"remote-complete" },
        "localLabel":"Local snapshot","remoteLabel":"Remote snapshot"
    }))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::{
        contract::{ObjectReceipt, ObjectRole, RemoteLocator},
        control::HeadDocument,
        packaging::RemoteObject,
    };
    use risunest_external_storage_format::{
        format::Strategy,
        snapshot as wire,
    };

    fn descriptor() -> Descriptor {
        Descriptor::new("descriptor-repository".into(), Some(Strategy::Cas),
        )
        .unwrap()
    }
    fn identity(revision: i64) -> CaptureIdentity {
        CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "epoch".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision,
        }
    }
    fn snapshot() -> RemoteObject {
        let header = wire::PublicObjectHeader::new(
            "descriptor-repository".into(),
            "snapshot-s1".into(),
            wire::ObjectRole::SyncState,
            4,
        )
        .unwrap();
        RemoteObject {
            repository_id: "descriptor-repository".into(),
            object_id: "snapshot-s1".into(),
            role: ObjectRole::SyncState,
            receipt: ObjectReceipt {
                locator: RemoteLocator {
                    connection_identity: "provider-root".into(),
                    collection: None,
                    object: "opaque".into(),
                },
                byte_length: wire::envelope_length(&header).unwrap(),
                version: None,
                checksum: None,
                complete: true,
            },
            ciphertext_sha256: "11".repeat(32),
            plaintext_length: 4,
            plaintext_sha256: "22".repeat(32),
        }
    }
    fn remote(commit: &str, fingerprint: &str, body: u8, version: &str) -> ObservedHead {
        let document = HeadDocument::new(
            &descriptor(),
            "library".into(),
            commit.into(),
            None,
            fingerprint.into(),
            snapshot(),
        )
        .unwrap();
        ObservedHead {
            document,
            observation: HeadObservation {
                commit_id: commit.into(),
                authenticated_body_hash: format!("{body:02x}").repeat(32),
                version: Some(VersionToken(version.into())),
            },
        }
    }
    fn base(identity: CaptureIdentity, head: &ObservedHead) -> ExternalBase {
        ExternalBase {
            repository_id: "descriptor-repository".into(),
            snapshot_id: "s0".into(),
            commit_id: head.observation.commit_id.clone(),
            head_observation: observation_json(&head.observation).unwrap(),
            identity,
        }
    }

    /// Invariant 30. A device value that moved on its own publishes; it never
    /// turns into a library disagreement.
    #[test]
    fn a_section_only_change_publishes_instead_of_becoming_a_library_conflict() {
        let remote = remote("remote", "55".repeat(32).as_str(), 1, "1");
        let base = base(identity(4), &remote);
        assert_eq!(
            decide_sync(SyncInputs {
                descriptor: &descriptor(),
                connection_id: "connection",
                current_identity: &identity(4),
                local_pristine: false,
                local_fingerprint: None,
                local_sections_changed: true,
                base: Some(&base),
                remote: Some(&remote),
            })
            .unwrap(),
            SyncAction::PublishLocal {
                expected: Some(remote.clone())
            }
        );
        assert_eq!(
            decide_sync(SyncInputs {
                descriptor: &descriptor(),
                connection_id: "connection",
                current_identity: &identity(4),
                local_pristine: false,
                local_fingerprint: None,
                local_sections_changed: false,
                base: Some(&base),
                remote: Some(&remote),
            })
            .unwrap(),
            SyncAction::UpToDate
        );
    }

    #[test]
    fn first_attach_never_overwrites_an_existing_remote_head() {
        let remote = remote("remote", "44".repeat(32).as_str(), 1, "1");
        assert!(matches!(
            decide_sync(SyncInputs {
                descriptor: &descriptor(),
                connection_id: "connection",
                current_identity: &identity(1),
                local_pristine: false,
                local_fingerprint: None,
                local_sections_changed: false,
                base: None,
                remote: Some(&remote)
            })
            .unwrap(),
            SyncAction::FirstAttachDecision { .. }
        ));
        assert_eq!(
            decide_sync(SyncInputs {
                descriptor: &descriptor(),
                connection_id: "connection",
                current_identity: &identity(1),
                local_pristine: false,
                local_fingerprint: None,
                local_sections_changed: false,
                base: None,
                remote: None
            })
            .unwrap(),
            SyncAction::PublishLocal { expected: None }
        );
        assert!(matches!(
            decide_sync(SyncInputs {
                descriptor: &descriptor(),
                connection_id: "connection",
                current_identity: &identity(0),
                local_pristine: true,
                local_fingerprint: None,
                local_sections_changed: false,
                base: None,
                remote: Some(&remote)
            })
            .unwrap(),
            SyncAction::ReceiveRemote { .. }
        ));
    }
    #[test]
    fn local_remote_and_two_sided_changes_are_distinguished() {
        let old = remote("old", "33".repeat(32).as_str(), 1, "1");
        let base = base(identity(1), &old);
        assert_eq!(
            decide_sync(SyncInputs {
                descriptor: &descriptor(),
                connection_id: "c",
                current_identity: &identity(2),
                local_pristine: false,
                local_fingerprint: Some(&"55".repeat(32)),
                local_sections_changed: false,
                base: Some(&base),
                remote: Some(&old)
            })
            .unwrap(),
            SyncAction::PublishLocal {
                expected: Some(old.clone())
            }
        );
        let new = remote("new", "44".repeat(32).as_str(), 2, "2");
        assert_eq!(
            decide_sync(SyncInputs {
                descriptor: &descriptor(),
                connection_id: "c",
                current_identity: &identity(1),
                local_pristine: false,
                local_fingerprint: None,
                local_sections_changed: false,
                base: Some(&base),
                remote: Some(&new)
            })
            .unwrap(),
            SyncAction::ReceiveRemote {
                remote: new.clone()
            }
        );
        assert_eq!(
            decide_sync(SyncInputs {
                descriptor: &descriptor(),
                connection_id: "c",
                current_identity: &identity(2),
                local_pristine: false,
                local_fingerprint: Some(&"66".repeat(32)),
                local_sections_changed: false,
                base: Some(&base),
                remote: Some(&new)
            })
            .unwrap(),
            SyncAction::PreserveConflict {
                remote: new.clone()
            }
        );
        assert_eq!(
            decide_sync(SyncInputs {
                descriptor: &descriptor(),
                connection_id: "c",
                current_identity: &identity(2),
                local_pristine: false,
                local_fingerprint: Some(&"44".repeat(32)),
                local_sections_changed: false,
                base: Some(&base),
                remote: Some(&new)
            })
            .unwrap(),
            SyncAction::AcceptEquivalent { remote: new }
        );
    }
    #[test]
    fn missing_known_head_and_replaced_local_lineage_require_explicit_recovery() {
        let old = remote("old", "33".repeat(32).as_str(), 1, "1");
        let base = base(identity(1), &old);
        assert_eq!(
            decide_sync(SyncInputs {
                descriptor: &descriptor(),
                connection_id: "c",
                current_identity: &identity(1),
                local_pristine: false,
                local_fingerprint: None,
                local_sections_changed: false,
                base: Some(&base),
                remote: None
            })
            .unwrap(),
            SyncAction::RecoveryRequired
        );
        let mut replaced = identity(2);
        replaced.library_epoch = "replacement".into();
        assert_eq!(
            decide_sync(SyncInputs {
                descriptor: &descriptor(),
                connection_id: "c",
                current_identity: &replaced,
                local_pristine: false,
                local_fingerprint: None,
                local_sections_changed: false,
                base: Some(&base),
                remote: Some(&old)
            })
            .unwrap(),
            SyncAction::DecisionRequired
        );
    }
}
