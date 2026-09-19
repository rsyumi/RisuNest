//! Bounded sync decision and publication orchestration.
//!
//! The decision is pure: callers obtain one authenticated remote head and one
//! short-lived PDS snapshot, then perform the selected network or activation
//! stage. No branch silently changes a repository's fixed publication strategy.
use super::{
    connection_commands::ConnectedRepository,
    contract::{Cancellation, ErrorKind, ProviderError, Result, VersionToken},
    control::{self, HeadDocument, ObservedHead, PublicationResult},
    job_store::{DurableJob, JobKind, JobStore},
    journal::{JobIdentity, TransferJournal},
    packaging::{CompletedSnapshot, PackageLimits, SnapshotMetadata, SnapshotPurpose},
    publication::HeadObservation,
};
use crate::persistent_store::{
    device_store::Section as PdsSection,
    external_apply::{ExternalSnapshotApplication, ExternalSnapshotObject, ExternalSnapshotRecord},
    external_conflicts::{
        self, ExternalConflictRecord, PreservedHeadObservation, PreservedRemoteState,
    },
    external_runtime::ExternalBase,
    sync_selection::CaptureIdentity,
    PersistentStore, PreparedReplaceCommit,
};
use risunest_external_storage_format::{
    format::{library_fingerprint_domain, Descriptor},
    section::SectionKind,
};
use risunest_sync_wire::head::Sequence;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};

type ReceiveParticipation = Vec<(PdsSection, bool, Sequence)>;

pub(crate) struct PreparedReceive {
    job_id: String,
    connection_id: String,
    snapshot_id: String,
    expected: CaptureIdentity,
    authenticated_head: String,
    commit: PreparedReplaceCommit,
    participation: ReceiveParticipation,
    sections: Vec<super::sections::PreparedSectionInput>,
}
impl PreparedReceive {
    fn ready_result(&self) -> Value {
        json!({"receiveReady":true,"snapshotId":self.snapshot_id,
            "expectedRevision":self.expected.revision.to_string()})
    }
}

pub(crate) fn prepared_receive_result(app: &AppHandle, job: &str) -> Result<Option<Value>> {
    let state = app.state::<super::job_store::JobCommandState>();
    let prepared = state.prepared_receives.lock().map_err(local_error)?;
    Ok(prepared.get(job).map(PreparedReceive::ready_result))
}

pub(crate) fn discard_receive_preparation(app: &AppHandle, id: &str) -> Result<()> {
    let state = app.state::<super::job_store::JobCommandState>();
    let prepared = state.prepared_receives.lock().map_err(local_error)?.remove(id);
    let jobs = JobStore::open(&super::runtime::root(app)?)?;
    let mut job = jobs.read(id)?;
    let staging_id = prepared.as_ref().map(|entry| entry.commit.external_staging_id())
        .or(job.receive_staging_id.as_deref());
    if let Some(staging_id) = staging_id {
        pds(app)?.replace_abort(staging_id).map_err(local_error)?;
    }
    if job.receive_staging_id.take().is_some() {
        jobs.put(&job)?;
    }
    Ok(())
}

fn require_exact_receive_identity(expected: &CaptureIdentity, current: &CaptureIdentity) -> Result<()> {
    if expected != current {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(())
}

fn receive_participation(store: &mut PersistentStore) -> Result<ReceiveParticipation> {
    let device = store.device_store_mut().map_err(local_error)?;
    [PdsSection::Hypa, PdsSection::LocalPlugins].into_iter().map(|section| {
        let state = device.section_state(section).map_err(local_error)?;
        Ok((section, state.participating, state.participation_generation))
    }).collect()
}

fn require_receive_participation(store: &mut PersistentStore, expected: &ReceiveParticipation) -> Result<()> {
    if receive_participation(store)? != *expected {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(())
}

fn wanted_receive_sections(participation: &ReceiveParticipation) -> std::collections::BTreeSet<String> {
    participation.iter().filter_map(|(section, enabled, _)| {
        if !enabled { return None; }
        match section {
            PdsSection::Hypa => Some(SectionKind::Hypa.id().to_owned()),
            PdsSection::LocalPlugins => Some(SectionKind::LocalPlugins.id().to_owned()),
        }
    }).collect()
}

fn prepare_receive_sections(
    connection_id: &str,
    library_lineage: &str,
    participation: &ReceiveParticipation,
    sections: Vec<super::sections::CapturedSection>,
    cancel: &Cancellation,
) -> Result<Vec<super::sections::PreparedSectionInput>> {
    let mut seen = std::collections::BTreeSet::new();
    sections.into_iter().map(|section| {
        cancel.check()?;
        if !seen.insert(section.kind.id().to_owned()) {
            return Err(corrupt("received section is duplicated"));
        }
        let device_section = super::sections::section_of(section.kind)
            .ok_or_else(|| corrupt("device-fixed section in a synchronized state"))?;
        let (_, participating, generation) = participation.iter()
            .find(|(candidate, _, _)| *candidate == device_section)
            .ok_or_else(|| corrupt("received section has no participation identity"))?;
        if !participating {
            return Err(corrupt("received section is not participating"));
        }
        super::sections::prepare_received_section(
            connection_id,
            library_lineage,
            super::sections::SectionArrival::Continuing,
            generation,
            &section,
            cancel,
        )
    }).collect()
}

fn prepare_receive_input(
    store: &mut PersistentStore,
    job: &DurableJob,
    expected: CaptureIdentity,
    authenticated_head: String,
    downloaded: super::snapshot_restore::PreparedRemoteSnapshot,
    sections: Vec<super::sections::CapturedSection>,
    participation: ReceiveParticipation,
    cancel: &Cancellation,
) -> Result<PreparedReceive> {
    require_exact_receive_identity(&expected, &store.external_identity().map_err(local_error)?)?;
    store.external_validate_receive(&job.id, &job.request.connection_id, &expected, &authenticated_head)
        .map_err(|_| ProviderError::new(ErrorKind::PreconditionFailed))?;
    require_receive_participation(store, &participation)?;
    let intent = store.external_job(&job.id).map_err(local_error)?
        .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
    if intent.capture_id != downloaded.snapshot_id || intent.repository_id != downloaded.repository_id {
        return Err(corrupt("received snapshot binding differs"));
    }
    let wanted = wanted_receive_sections(&participation);
    if sections.iter().any(|section| !wanted.contains(section.kind.id())) {
        return Err(corrupt("received section is not participating"));
    }
    let sections = prepare_receive_sections(
        &job.request.connection_id,
        &expected.library_epoch,
        &participation,
        sections,
        cancel,
    )?;
    let scope_id = library_fingerprint_domain();
    let fingerprint = hex::decode(&downloaded.library_fingerprint)
        .ok().and_then(|value| value.try_into().ok())
        .ok_or_else(|| corrupt("invalid received fingerprint"))?;
    let application = ExternalSnapshotApplication {
        expected_revision: expected.revision,
        staging_root: &downloaded.staging_root,
        scope_id: &scope_id,
        fingerprint: &fingerprint,
    };
    let records = downloaded.records.into_iter().map(|record| Ok(ExternalSnapshotRecord {
        key: record.key, content_hash: record.content_hash,
        byte_length: record.byte_length, path: record.path,
    }));
    let objects = downloaded.objects.into_iter().map(|object| Ok(ExternalSnapshotObject {
        content_hash: object.content_hash, byte_length: object.byte_length, path: object.path,
    }));
    let commit = store.prepare_external_snapshot_application(&application, records, objects)
        .map_err(local_error)?;
    Ok(PreparedReceive {
        job_id: job.id.clone(), connection_id: job.request.connection_id.clone(),
        snapshot_id: downloaded.snapshot_id, expected, authenticated_head, commit,
        participation, sections,
    })
}

fn receive_validation_error(error: crate::persistent_store::StoreError) -> ProviderError {
    match error {
        crate::persistent_store::StoreError::Store { .. } => local_error(error),
        _ => ProviderError::new(ErrorKind::PreconditionFailed),
    }
}

// No transport or credentials enter the activation boundary. A failed attempt
// retains the original staged generation and section inputs for the same job.
fn activate_prepared_receive(
    store: &mut PersistentStore,
    prepared: &PreparedReceive,
    current: &CaptureIdentity,
) -> Result<i64> {
    let staging_id = prepared.commit.external_staging_id();
    let result = (|| {
        require_exact_receive_identity(&prepared.expected, current)?;
        store.external_validate_receive(&prepared.job_id, &prepared.connection_id,
            &prepared.expected, &prepared.authenticated_head)
            .map_err(receive_validation_error)?;
        require_receive_participation(store, &prepared.participation)?;
        // This only revalidates the existing SQL stage; it neither downloads
        // nor materializes the library and never rebases the expected revision.
        let commit = store.prepare_replace_commit(staging_id, Some(prepared.expected.revision))
            .map_err(receive_validation_error)?;
        for section in &prepared.sections {
            super::sections::apply_prepared_section(store, section)?;
        }
        store.finish_external_receive(commit, &prepared.job_id)
            .map(|result| result.revision).map_err(local_error)
    })();
    if let Err(error) = &result {
        if let Some(completed) = store.external_receive_completion(&prepared.job_id, &prepared.connection_id)
            .map_err(local_error)?
        {
            return Ok(completed.revision);
        }
        if matches!(error.kind, ErrorKind::PreconditionFailed | ErrorKind::Corrupt)
            && store.replace_abort(staging_id).is_err()
        {
            crate::nlog!("error", "Rejected external receive stage could not be removed");
        }
    }
    result
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
            Ok(SyncAction::PreserveConflict {
                remote: remote.clone(),
            })
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
        let publication_permit =
            super::runtime::publication_permit(&worker_app, &worker_job.id)?;
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
        store
            .external_prepare_publication(&intent, &publication_permit)
            .map_err(local_error)?;
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

#[allow(clippy::too_many_arguments)]
async fn package_capture(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    capture: crate::persistent_store::external_capture::CapturedSnapshot,
    fingerprint: [u8; 32],
    expected: Option<&ObservedHead>,
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
    let parent = match expected {
        Some(head) => Some(
            control::read_snapshot_document(connected, &head.document.state, cancel).await?,
        ),
        None => None,
    };
    let generation = match &parent {
        Some(view) => Sequence::try_from(view.revision.clone())
            .map_err(corrupt)?
            .next()
            .map_err(corrupt)?,
        None => Sequence::from(1u64),
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
        purpose: SnapshotPurpose::SyncState {
            epoch: identity.generation.clone(),
            generation,
            parent_sections: parent.map(|view| view.sections).unwrap_or_default(),
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
    match super::packaging::verify_publication(
        &completed,
        &cache,
        &mut journal,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    ).await? {
        super::packaging::PublicationReadiness::Verified => {}
        super::packaging::PublicationReadiness::Repackage => {
            return Err(ProviderError::new(ErrorKind::Transient));
        }
    }
    Ok((completed, journal, publications))
}

async fn receive_remote(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    expected: &CaptureIdentity,
    remote: &ObservedHead,
    cancel: &Cancellation,
) -> Result<Value> {
    cancel.check()?;
    remember_discovery(
        app,
        &job.request.connection_id,
        remote.document.state.object_id.trim_start_matches("snapshot-"),
        &remote.document.state,
    );
    let _admission = app.state::<crate::native_file_jobs::NativeFileJobState>()
        .admission.file(false).map_err(local_error)?;
    let (observation, participation) = {
        let mut store = pds(app)?;
        require_exact_receive_identity(expected, &store.external_identity().map_err(local_error)?)?;
        super::runtime::require_admitted_library(job, expected)?;
        let existing = store.external_job(&job.id).map_err(local_error)?;
        let observation = if let Some(pending) = existing.as_ref()
            .filter(|item| item.role == "restore" && item.phase == "ready")
        {
            let original = pending.expected_head.as_deref()
                .ok_or_else(|| corrupt("missing received head observation"))?;
            require_received_head(original, Some(&remote.observation))?;
            original.to_owned()
        } else {
            observation_json(&remote.observation)?
        };
        let intent = crate::persistent_store::external_storage_state::ReceiveIntent {
            job_id: &job.id,
            connection_id: &job.request.connection_id,
            repository_id: &connected.stored.descriptor.repository_id,
            snapshot_id: remote.document.state.object_id.trim_start_matches("snapshot-"),
            commit_id: &remote.document.commit_id,
            authenticated_head: &observation,
            identity: expected,
        };
        if job.request.kind == JobKind::ResolveConflict {
            if existing.as_ref().is_some_and(|item| matches!(item.phase.as_str(), "stale" | "cancelled")) {
                store.external_prepare_receive(&intent).map_err(local_error)?;
            }
        } else if existing.is_none() {
            store.external_prepare_receive(&intent).map_err(local_error)?;
        }
        store.external_validate_receive(&job.id, &job.request.connection_id, expected, &observation)
            .map_err(|_| ProviderError::new(ErrorKind::PreconditionFailed))?;
        (observation, receive_participation(&mut store)?)
    };
    // A process restart loses the native commit handle. Only this worker may
    // revalidate the downloaded files and build another inactive generation.
    discard_receive_preparation(app, &job.id)?;
    let root = super::runtime::root(app)?;
    let staging = super::runtime::job_directory(&root, &job.request.connection_id, &job.id).join("receive");
    let downloaded = super::snapshot_restore::download_snapshot(
        &remote.document.state, &staging, &connected.root_key,
        connected.provider.as_ref(), &connected.handle, cancel,
    ).await?;
    cancel.check()?;
    let sections = super::snapshot_restore::download_sections(
        &remote.document.state, &wanted_receive_sections(&participation), &staging,
        &connected.root_key, connected.provider.as_ref(), &connected.handle, cancel,
    ).await?;
    let worker_app = app.clone();
    let worker_job = job.clone();
    let worker_identity = expected.clone();
    let worker_head = observation;
    let worker_cancel = cancel.clone();
    let prepared = tokio::task::spawn_blocking(move || {
        worker_cancel.check()?;
        super::runtime::read_job_session(&worker_app, &worker_job.id)?;
        prepare_receive_input(&mut pds(&worker_app)?, &worker_job, worker_identity,
            worker_head, downloaded, sections, participation, &worker_cancel)
    }).await.map_err(local_error)??;
    let recorded = (|| {
        let jobs = JobStore::open(&root)?;
        let mut durable = jobs.read(&job.id)?;
        durable.receive_staging_id = Some(prepared.commit.external_staging_id().to_owned());
        jobs.put(&durable)
    })();
    if let Err(error) = recorded {
        if pds(app).and_then(|mut store| store.replace_abort(prepared.commit.external_staging_id())
            .map_err(local_error)).is_err()
        {
            crate::nlog!("error", "Unrecorded external receive stage could not be removed");
        }
        return Err(error);
    }
    let checked = async {
        let current = control::read_head(
            connected.provider.as_ref(), &connected.handle, &connected.stored.descriptor,
            &connected.root_key, None, cancel,
        ).await?;
        require_received_head(&prepared.authenticated_head,
            current.as_ref().map(|head| &head.observation))?;
        {
            let mut store = pds(app)?;
            require_exact_receive_identity(&prepared.expected, &store.external_identity().map_err(local_error)?)?;
            store.external_validate_receive(&job.id, &job.request.connection_id,
                &prepared.expected, &prepared.authenticated_head)
                .map_err(|_| ProviderError::new(ErrorKind::PreconditionFailed))?;
            require_receive_participation(&mut store, &prepared.participation)?;
        }
        super::runtime::read_job_session(app, &job.id)?;
        let state = app.state::<super::job_store::JobCommandState>();
        let active = state.active.lock().map_err(local_error)?;
        let (connection, owner_cancel) = active.get(&job.id)
            .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
        if connection != &job.request.connection_id {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        owner_cancel.check()?;
        cancel.check()?;
        let result = prepared.ready_result();
        let mut entries = state.prepared_receives.lock().map_err(local_error)?;
        if entries.contains_key(&job.id) {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        entries.insert(job.id.clone(), prepared);
        Ok(result)
    }.await;
    if checked.is_err() {
        if discard_receive_preparation(app, &job.id).is_err() {
            crate::nlog!("error", "External receive preparation could not be discarded");
        }
    }
    checked
}

fn require_received_head(expected: &str, observed: Option<&HeadObservation>) -> Result<()> {
    let expected = parse_observation(expected)?;
    if !observed.is_some_and(|current| {
        current.commit_id == expected.commit_id
            && current.authenticated_body_hash == expected.authenticated_body_hash
    }) {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ApplyReceivedRequest {
    job_id: String,
    expected_revision: String,
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
    note_sections_published_in_store(
        &mut store,
        connection_id,
        library_lineage,
        sections,
        publications,
    )
}

fn note_sections_published_in_store(
    store: &mut PersistentStore,
    connection_id: &str,
    library_lineage: &str,
    sections: &std::collections::BTreeMap<
        String,
        risunest_external_storage_format::snapshot::SectionSnapshotRef,
    >,
    publications: &[super::sections::SectionPublication],
) -> Result<()> {
    let device = store.device_store_mut().map_err(local_error)?;
    for publication in publications {
        let mut matching = sections.values().filter(|reference| {
            super::sections::section_of(reference.kind) == Some(publication.section)
        });
        let reference = matching.next()
            .ok_or_else(|| corrupt("published section is absent from the confirmed state"))?;
        if matching.next().is_some() {
            return Err(corrupt("confirmed state repeats a published section"));
        }
        super::sections::note_prepared_section_published(
            device,
            publication,
            connection_id,
            library_lineage,
            reference,
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

fn receive_revision(request: &ApplyReceivedRequest) -> Result<i64> {
    let revision = request.expected_revision.parse::<i64>()
        .map_err(|_| corrupt("invalid staged receive revision"))?;
    if request.job_id.is_empty() || request.job_id.len() > 1024 || request.job_id.contains('\0')
        || revision < 0 || revision.to_string() != request.expected_revision
    {
        return Err(corrupt("invalid staged receive request"));
    }
    Ok(revision)
}

fn completed_receive_result(store: &PersistentStore, job: &DurableJob, expected: i64) -> Result<Option<Value>> {
    let Some(completed) = store.external_receive_completion(&job.id, &job.request.connection_id)
        .map_err(local_error)? else { return Ok(None) };
    if completed.expected_revision != expected {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(Some(json!({"snapshotId":completed.snapshot_id,
        "receivedRevision":completed.revision.to_string()})))
}

fn take_prepared_receive(
    state: &super::job_store::JobCommandState,
    claim: &super::job_store::JobClaim,
    job: &DurableJob,
    expected: i64,
) -> Result<PreparedReceive> {
    claim.require_job(state, job)?;
    let active = state.active.lock().map_err(local_error)?;
    let (connection, cancel) = active.get(&job.id)
        .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
    if connection != &job.request.connection_id {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    cancel.check()?;
    let mut entries = state.prepared_receives.lock().map_err(local_error)?;
    let entry = entries.get(&job.id)
        .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
    if entry.job_id != job.id || entry.connection_id != job.request.connection_id
        || entry.expected.revision != expected
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(entries.remove(&job.id).expect("prepared receive held under mutex"))
}

fn commit_prepared_receive(
    store: &mut PersistentStore,
    state: &super::job_store::JobCommandState,
    claim: &super::job_store::JobClaim,
    job: &DurableJob,
    expected: i64,
    current: &CaptureIdentity,
) -> Result<(String, i64)> {
    let prepared = take_prepared_receive(state, claim, job, expected)?;
    let snapshot_id = prepared.snapshot_id.clone();
    match activate_prepared_receive(store, &prepared, current) {
        Ok(revision) => Ok((snapshot_id, revision)),
        Err(error) => {
            if !matches!(error.kind, ErrorKind::PreconditionFailed | ErrorKind::Corrupt) {
                // The claim excludes another preparation, cancellation, or
                // apply until this original input has been restored.
                state.prepared_receives.lock().map_err(local_error)?
                    .insert(job.id.clone(), prepared);
            }
            Err(error)
        }
    }
}

fn apply_received(app: &AppHandle, request: &ApplyReceivedRequest) -> Result<Value> {
    let expected = receive_revision(request)?;
    let jobs = JobStore::open(&super::runtime::root(app)?)?;
    let mut job = jobs.read(&request.job_id)?;
    if !matches!(job.request.kind, JobKind::Sync | JobKind::ResolveConflict) {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    if let Some(result) = completed_receive_result(&pds(app)?, &job, expected)? {
        return Ok(result);
    }
    let state = app.state::<super::job_store::JobCommandState>();
    let (cancel, claim) = state.claim(&job)?;
    super::runtime::read_job_session(app, &job.id)?;
    let permit = app.state::<crate::native_file_jobs::NativeFileJobState>()
        .admission.file(true).map_err(local_error)?;
    let mut store = pds(app)?;
    // A command admitted after another apply observes that original commit.
    if let Some(result) = completed_receive_result(&store, &job, expected)? {
        return Ok(result);
    }
    let current = store.external_identity().map_err(local_error)?;
    cancel.check()?;
    super::runtime::read_job_session(app, &job.id)?;
    let outcome = commit_prepared_receive(&mut store, &state, &claim, &job, expected, &current);
    drop(store);
    drop(permit);
    let retained = state.prepared_receives.lock().map_err(local_error)?;
    job.receive_staging_id = retained.get(&job.id)
        .map(|prepared| prepared.commit.external_staging_id().to_owned());
    let ready_result = retained.get(&job.id).map(PreparedReceive::ready_result);
    drop(retained);
    let (snapshot_id, revision) = match outcome {
        Ok(result) => result,
        Err(error) => {
            let permanent = ready_result.is_none()
                && matches!(error.kind, ErrorKind::PreconditionFailed | ErrorKind::Corrupt);
            let mut preserving = false;
            if permanent {
                let mut store = pds(app)?;
                let cancelled = store.external_cancel_prepared(&job.id);
                if cancelled.is_err() {
                    crate::nlog!("error", "Rejected external receive intent could not be settled");
                }
                preserving = conflict_record(app, &job.id)?
                    .is_some_and(|record| !record.resolved && record.remote_point.is_some());
            }
            job.summary["state"] = json!(if preserving { "conflict" } else if permanent { "failed" } else { "waiting" });
            job.summary["phase"] = json!(if preserving { "conflict-choice" } else { "paused" });
            job.summary.as_object_mut().unwrap().remove("result");
            if let Some(ready) = ready_result {
                job.summary["phase"] = json!("remote-apply");
                job.summary["result"] = ready;
            } else if preserving {
                job.summary["result"] = json!({"conflictId":job.id});
            }
            job.summary["error"] = super::runtime::error_dto(&error);
            job.summary["updatedAtMs"] = json!(super::runtime::now_ms().to_string());
            if jobs.put(&job).is_err() {
                crate::nlog!("error", "External receive reprepare state could not be persisted");
            }
            return Err(error);
        }
    };
    if job.request.kind == JobKind::ResolveConflict {
        if mark_conflict_resolved(app, &job.id).is_err() {
            crate::nlog!("error", "External receive committed but conflict bookkeeping did not finish");
        }
    }
    super::runtime::record_connection_completion(
        app,
        &job.request.connection_id,
        super::connection_store::CompletionKind::Sync,
    );
    let result = json!({"snapshotId":snapshot_id,"receivedRevision":revision.to_string()});
    job.summary["state"] = json!("succeeded");
    job.summary["phase"] = json!("complete");
    job.summary["result"] = result.clone();
    job.summary.as_object_mut().unwrap().remove("error");
    job.summary["updatedAtMs"] = json!(super::runtime::now_ms().to_string());
    if jobs.put(&job).is_err() {
        crate::nlog!("error", "External receive committed but its UI summary could not be persisted");
    }
    // Terminal leases are reconciled by the next repository worker. Activation
    // neither opens credentials nor starts a provider request, even for cleanup.
    Ok(result)
}

#[tauri::command]
pub(crate) async fn external_storage_apply_received(
    app: AppHandle,
    request: ApplyReceivedRequest,
) -> Result<Value> {
    tokio::task::spawn_blocking(move || apply_received(&app, &request))
        .await.map_err(local_error)?
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
    if intent.phase == "publishing" {
        pds(app)?
            .external_publication_unknown(&job.id)
            .map_err(local_error)?;
    }
    let intended_state = format!("snapshot-{}", job.snapshot_id);
    match observe_unknown_publication(
        connected.provider.as_ref(),
        &connected.handle,
        &connected.stored.descriptor,
        &connected.root_key,
        &intent.commit_id,
        &intended_state,
        cancel,
    )
    .await?
    {
        Some(observed) => {
            remember_discovery(
                app,
                &job.request.connection_id,
                &job.snapshot_id,
                &observed.document.state,
            );
            let confirmed = control::read_snapshot_document(
                connected,
                &observed.document.state,
                cancel,
            )
            .await?;
            let observation = observation_json(&observed.observation)?;
            let permit = super::runtime::publication_permit(app, &job.id)?;
            let sections_spool = super::runtime::job_directory(
                &super::runtime::root(app)?,
                &job.request.connection_id,
                &job.id,
            )
            .join("sections");
            let publications =
                super::sections::load_prepared_section_publications(&sections_spool)?;
            note_sections_published(
                app,
                &job.request.connection_id,
                &intent.identity.library_epoch,
                &confirmed.sections,
                &publications,
            )?;
            pds(app)?
                .external_confirm_publication(
                    &permit,
                    &intent.commit_id,
                    &job.snapshot_id,
                    &observation,
                )
                .map_err(local_error)?;
            super::runtime::record_connection_completion(
                app,
                &job.request.connection_id,
                super::connection_store::CompletionKind::Sync,
            );
            return Ok(Some(
                json!({"snapshotId":job.snapshot_id,"publishedRevision":intent.identity.revision.to_string()}),
            ));
        }
        None => {}
    }
    Ok(Some(
        json!({"stopReason":"uncertain","reason":"publication-unknown"}),
    ))
}

async fn observe_unknown_publication(
    provider: &dyn super::contract::Provider,
    repository: &super::contract::RepositoryHandle,
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    intended_commit: &str,
    intended_state: &str,
    cancel: &Cancellation,
) -> Result<Option<ObservedHead>> {
    for attempt in 0..2 {
        cancel.check()?;
        match control::read_head(
            provider,
            repository,
            descriptor,
            root_key,
            None,
            cancel,
        )
        .await
        {
            Ok(observed) => {
                let authenticated_head = observed.as_ref().map(|head| {
                    (
                        head.document.commit_id.as_str(),
                        head.document.state.object_id.as_str(),
                    )
                });
                if super::publication::classify_publication(
                    intended_commit,
                    intended_state,
                    authenticated_head,
                    false,
                ) == super::publication::PublicationObservation::Confirmed
                {
                    return Ok(observed);
                }
            }
            Err(error) => {
                let retryable = matches!(
                    error.kind,
                    ErrorKind::Transient
                        | ErrorKind::RateLimited
                        | ErrorKind::DailyQuotaExhausted
                );
                if !retryable || attempt == 1 {
                    return Err(error);
                }
            }
        }
    }
    Ok(None)
}

async fn preserve_conflict(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    remote: ObservedHead,
    capture: crate::persistent_store::external_capture::CapturedSnapshot,
    _fingerprint: [u8; 32],
    protection: Option<&super::runtime::RepositoryProtection<'_>>,
    cancel: &Cancellation,
) -> Result<Value> {
    let local = capture
        .durable_reference(&super::runtime::root(app)?)
        .map_err(local_error)?;
    let record = conflict_record_from_remote(connected, job, local, &remote, cancel).await?;
    let record = preserve_local_conflict(app, job, record)?;
    ensure_conflict_point(app, connected, job, record, false, protection, cancel).await
}

async fn conflict_record_from_remote(
    connected: &ConnectedRepository,
    job: &DurableJob,
    local: super::capture::DurableCaptureReference,
    remote: &ObservedHead,
    cancel: &Cancellation,
) -> Result<ExternalConflictRecord> {
    let view = control::read_snapshot_document(connected, &remote.document.state, cancel).await?;
    Ok(ExternalConflictRecord {
        id: job.id.clone(),
        created_at_ms: i64::try_from(super::runtime::now_ms()).map_err(corrupt)?,
        connection_id: job.request.connection_id.clone(),
        repository_id: connected.stored.descriptor.repository_id.clone(),
        local,
        remote: PreservedRemoteState {
            snapshot: remote.document.state.stored(&connected.handle)?,
            logical_revision: view.revision.parse().map_err(corrupt)?,
            commit_id: remote.document.commit_id.clone(),
            head: PreservedHeadObservation {
                commit_id: remote.document.commit_id.clone(),
                authenticated_body_hash: remote.observation.authenticated_body_hash.clone(),
            },
        },
        remote_point: None,
        resolved: false,
    })
}

fn preserve_local_conflict(
    app: &AppHandle,
    job: &DurableJob,
    record: ExternalConflictRecord,
) -> Result<ExternalConflictRecord> {
    let owner = external_conflicts::conflict_capture_owner(&record.id).map_err(local_error)?;
    let mut store = pds(app)?;
    let existing = external_conflicts::external_conflict(
        store.device_store().map_err(local_error)?.connection(),
        &record.id,
    )
    .map_err(local_error)?;
    let preserved = if existing.is_some() {
        external_conflicts::preserve_local_conflict(
            store.device_store().map_err(local_error)?.connection(),
            &record,
        )
        .map_err(local_error)?
    } else {
        store
            .retain_external_capture(&record.local.capture_id, &owner)
            .map_err(local_error)?;
        match external_conflicts::preserve_local_conflict(
            store.device_store().map_err(local_error)?.connection(),
            &record,
        ) {
            Ok(preserved) => preserved,
            Err(error) => {
                if store
                    .release_external_capture(&record.local.capture_id, &owner)
                    .is_err()
                {
                    crate::nlog!("error", "Failed to compensate a rejected conflict capture owner");
                }
                return Err(local_error(error));
            }
        }
    };
    if store
        .external_job(&job.id)
        .map_err(local_error)?
        .is_some_and(|intent| matches!(intent.phase.as_str(), "ready" | "publishing"))
    {
        store
            .external_publication_rejected(&job.id)
            .map_err(local_error)?;
    }
    store
        .release_external_capture(&record.local.capture_id, &job.id)
        .map_err(local_error)?;
    Ok(preserved)
}

fn conflict_journal(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    record: &ExternalConflictRecord,
) -> Result<TransferJournal> {
    let root = super::runtime::root(app)?;
    let directory = super::runtime::job_directory(&root, &job.request.connection_id, &job.id);
    TransferJournal::open(
        &directory,
        JobIdentity {
            job_id: job.id.clone(),
            connection_id: job.request.connection_id.clone(),
            repository_id: connected.handle.repository_id.clone(),
            capture_id: record.local.capture_id.clone(),
            capture: record.local.identity.clone(),
        },
    )
}

async fn resume_conflict_preservation(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    record: ExternalConflictRecord,
    protection: Option<&super::runtime::RepositoryProtection<'_>>,
    cancel: &Cancellation,
) -> Result<Value> {
    ensure_conflict_point(app, connected, job, record, false, protection, cancel).await
}

pub(crate) async fn ensure_conflict_point(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    record: ExternalConflictRecord,
    force_recheck: bool,
    protection: Option<&super::runtime::RepositoryProtection<'_>>,
    cancel: &Cancellation,
) -> Result<Value> {
    if record.connection_id != job.request.connection_id
        || record.repository_id != connected.stored.descriptor.repository_id
        || (record.resolved && !force_recheck)
    {
        return Err(corrupt("invalid local conflict preservation"));
    }
    if record.remote_point.is_some() && !force_recheck {
        return Ok(json!({"snapshotId":record.local.capture_id,"conflictId":job.id,"preservation":"remote-complete"}));
    }
    let mut journal = conflict_journal(app, connected, job, &record)?;
    let remote_snapshot = super::packaging::RemoteObject::from_stored(
        &record.remote.snapshot,
        &connected.handle,
    )?;
    let created_at_ms = u64::try_from(record.created_at_ms).map_err(corrupt)?;
    let remote_bundle = control::ensure_remote_conflict_bundle(
        connected,
        &record.id,
        &record.remote.commit_id,
        created_at_ms,
        &remote_snapshot,
        &mut journal,
        cancel,
    )
    .await?;
    if let Some(protection) = protection {
        protection.recheck(cancel).await?;
    }
    let point = control::ensure_remote_conflict_point(
        &connected.stored.descriptor,
        &record.id,
        created_at_ms,
        remote_bundle,
        &mut journal,
        connected,
        cancel,
    )
    .await?;
    let store = pds(app)?;
    external_conflicts::confirm_external_conflict_point(
        store.device_store().map_err(local_error)?.connection(),
        &record.id,
        &point.stored(&connected.handle)?,
    )
    .map_err(local_error)?;
    let _ = journal
        .release_completed_sessions(connected.dependencies.vault.as_ref())
        .await;
    Ok(json!({"snapshotId":record.local.capture_id,"conflictId":job.id,"preservation":"remote-complete"}))
}

pub(crate) fn conflict_record(app: &AppHandle, id: &str) -> Result<Option<ExternalConflictRecord>> {
    let store = pds(app)?;
    external_conflicts::external_conflict(
        store.device_store().map_err(local_error)?.connection(),
        id,
    )
    .map_err(local_error)
}

pub(crate) fn mark_conflict_resolved(app: &AppHandle, id: &str) -> Result<()> {
    let store = pds(app)?;
    external_conflicts::mark_external_conflict_resolved(
        store.device_store().map_err(local_error)?.connection(),
        id,
    )
    .map(|_| ())
    .map_err(local_error)
}

async fn run_resolve_conflict(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    protection: Option<&super::runtime::RepositoryProtection<'_>>,
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
    let stored = {
        let store = pds(app)?;
        external_conflicts::external_conflict_for_resolution(
            store.device_store().map_err(local_error)?.connection(),
            conflict_id,
        )
        .map_err(local_error)?
    };
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
    if stored.remote.head.commit_id != remote.document.commit_id
        || stored.remote.head.authenticated_body_hash
            != remote.observation.authenticated_body_hash
        || stored.remote.snapshot != remote.document.state.stored(&connected.handle)?
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    remember_discovery(
        app,
        &job.request.connection_id,
        remote.document.state.object_id.trim_start_matches("snapshot-"),
        &remote.document.state,
    );
    if choice == "remote" {
        let expected = pds(app)?.external_job(&job.id).map_err(local_error)?
            .filter(|intent| intent.role == "restore" && intent.phase == "ready")
            .map(|intent| intent.identity).unwrap_or(stored.local.identity.clone());
        return receive_remote(app, connected, job, &expected, &remote, cancel).await;
    }
    if choice != "local" {
        return Err(corrupt("invalid conflict choice"));
    }
    let capture = pds(app)?
        .reopen_external_capture(&stored.local.capture_id)
        .map_err(local_error)?;
    if capture.durable_reference(&super::runtime::root(app)?).map_err(local_error)?
        != stored.local
    {
        return Err(corrupt("conflict capture binding differs"));
    }
    let fingerprint = capture
        .catalog
        .content_fingerprint(&library_fingerprint_domain())
        .map_err(local_error)?;
    let base = {
        let store = pds(app)?;
        store
            .external_base(&job.request.connection_id)
            .map_err(local_error)?
    };
    if let Some(base) = base {
        rejoin_sections(
            app,
            connected,
            job,
            &stored.local.identity,
            &base,
            &remote,
            cancel,
        )
        .await?;
    }
    let strategy = connected
        .stored
        .descriptor
        .publication_strategy
        .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
    let commit_id = format!("resolve-{}", job.id);
    let (completed, mut resolve_journal, publications) = package_capture(
        app,
        connected,
        job,
        capture,
        fingerprint,
        Some(&remote),
        cancel,
    )
    .await?;
    let document = HeadDocument::new(
        &connected.stored.descriptor,
        remote.document.library_id.clone(),
        commit_id.clone(),
        Some(remote.document.commit_id.clone()),
        completed.fingerprint.clone(),
        completed.reference.clone(),
    )?;
    let prepared = control::prepare_head(
        &connected.stored.descriptor,
        &connected.root_key,
        &connected.handle,
        document,
    )?;
    let intent = crate::persistent_store::external_storage_state::PublishIntent {
        job_id: &job.id,
        connection_id: &job.request.connection_id,
        repository_id: &connected.stored.descriptor.repository_id,
        capture_id: &stored.local.capture_id,
        identity: &stored.local.identity,
        strategy: match strategy {
            risunest_external_storage_format::format::Strategy::Cas => "cas",
            risunest_external_storage_format::format::Strategy::Sequential => "sequential",
        },
        expected_head: Some(&observation),
        commit_id: &commit_id,
    };
    let _permit = app
        .state::<crate::native_file_jobs::NativeFileJobState>()
        .admission
        .file(false)
        .map_err(local_error)?;
    let prepare_permit = super::runtime::publication_permit(app, &job.id)?;
    pds(app)?
        .external_prepare_publication(&intent, &prepare_permit)
        .map_err(local_error)?;
    let mut publication_permit = None;
    if let Some(protection) = protection {
        protection.recheck(cancel).await?;
    }
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
            let permit = super::runtime::publication_permit(app, &job.id)?;
            if permit.mode() != live {
                return Err(ProviderError::new(ErrorKind::Cancelled));
            }
            pds(app)?
                .external_begin_publication(&permit)
                .map_err(local_error)?;
            publication_permit = Some(permit);
            Ok(())
        },
        cancel,
    )
    .await
    {
        Ok(result) => result,
        Err(error) => return Err(error),
    };
    match result {
        PublicationResult::Confirmed(head) => {
            remember_discovery(
                app,
                &job.request.connection_id,
                &completed.snapshot_id,
                &completed.reference,
            );
            let observation = observation_json(&head.observation)?;
            let permit = publication_permit
                .as_ref()
                .ok_or_else(|| corrupt("missing publication permit"))?;
            note_sections_published(
                app,
                &job.request.connection_id,
                &stored.local.identity.library_epoch,
                &completed.sections,
                &publications,
            )?;
            pds(app)?
                .external_confirm_publication(
                    permit,
                    &commit_id,
                    &completed.snapshot_id,
                    &observation,
                )
                .map_err(local_error)?;
            let _ = resolve_journal
                .release_completed_sessions(connected.dependencies.vault.as_ref())
                .await;
            if mark_conflict_resolved(app, conflict_id).is_err() {
                crate::nlog!("error","External conflict publication committed but conflict bookkeeping did not finish");
            }
            super::runtime::record_connection_completion(
                app,
                &job.request.connection_id,
                super::connection_store::CompletionKind::Sync,
            );
            Ok(
                json!({"snapshotId":completed.snapshot_id,"publishedRevision":stored.local.identity.revision.to_string()}),
            )
        }
        PublicationResult::Conflict(_) => {
            pds(app)?
                .external_publication_rejected(&job.id)
                .map_err(local_error)?;
            Err(ProviderError::new(ErrorKind::PreconditionFailed))
        }
        PublicationResult::Unknown { .. } => {
            pds(app)?
                .external_publication_unknown(&job.id)
                .map_err(local_error)?;
            Err(ProviderError::new(ErrorKind::Transient))
        }
    }
}

pub(crate) async fn run_sync(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    protection: Option<&super::runtime::RepositoryProtection<'_>>,
    cancel: &Cancellation,
) -> Result<Value> {
    if job.request.kind == JobKind::ResolveConflict {
        if let Some(result) = reconcile_unknown(app, connected, job, cancel).await? {
            if mark_conflict_resolved(app, &job.id).is_err() {
                crate::nlog!("error","External conflict publication reconciled but conflict bookkeeping did not finish");
            }
            return Ok(result);
        }
        return run_resolve_conflict(app, connected, job, protection, cancel).await;
    }
    if let Some(result) = reconcile_unknown(app, connected, job, cancel).await? {
        return Ok(result);
    }
    super::runtime::read_job_session(app, &job.id)?;
    let strategy = connected
        .stored
        .descriptor
        .publication_strategy
        .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
    connected.stored.capabilities.require(strategy)?;
    if let Some(record) = conflict_record(app, &job.id)?.filter(|record| !record.resolved) {
        return resume_conflict_preservation(app, connected, job, record, protection, cancel).await;
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
    if let Some(remote) = remote.as_ref() {
        remember_discovery(
            app,
            &job.request.connection_id,
            remote.document.state.object_id.trim_start_matches("snapshot-"),
            &remote.document.state,
        );
    }
    let store = pds(app)?;
    let identity = store.external_identity().map_err(local_error)?;
    let local_pristine = store.external_library_is_pristine().map_err(local_error)?;
    let base = store
        .external_base(&job.request.connection_id)
        .map_err(local_error)?;
    let pending_receive = store.external_job(&job.id).map_err(local_error)?
        .filter(|intent| intent.role == "restore" && intent.phase == "ready");
    drop(store);
    if let Some(intent) = pending_receive {
        let remote = remote.as_ref().ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
        if intent.repository_id != connected.stored.descriptor.repository_id
            || intent.commit_id != remote.document.commit_id
            || remote.document.state.object_id != format!("snapshot-{}", intent.capture_id)
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        return receive_remote(app, connected, job, &intent.identity, remote, cancel).await;
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
            receive_remote(app, connected, job, &identity, &remote, cancel).await
        }
        SyncAction::AcceptEquivalent { remote } => {
            if captured.is_some() {
                return Err(corrupt("equivalent state unexpectedly captured local changes"));
            }
            let publication_permit = super::runtime::publication_permit(app, &job.id)?;
            let base = base.ok_or_else(|| corrupt("equivalent state has no base"))?;
            let observation = observation_json(&remote.observation)?;
            pds(app)?
                .external_accept_equivalent(
                    &publication_permit,
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
                    &identity,
                )
                .map_err(local_error)?;
            Ok(
                json!({"snapshotId":remote.document.state.object_id.trim_start_matches("snapshot-"),"publishedRevision":identity.revision.to_string()}),
            )
        }
        SyncAction::PublishLocal { expected } => {
            let (capture, fingerprint) = captured
                .take()
                .ok_or_else(|| corrupt("missing publication capture"))?;
            let published_identity = capture.identity.clone();
            let local_reference = capture
                .durable_reference(&super::runtime::root(app)?)
                .map_err(local_error)?;
            // Rejoining changes device rows, so it belongs to publication, not
            // to a receive's read-only preparation or conflict preservation.
            if let (Some(base), Some(remote)) = (base.as_ref(), expected.as_ref()) {
                rejoin_sections(app, connected, job, &published_identity, base, remote, cancel).await?;
            }
            let (completed, mut journal, publications) = package_capture(
                app,
                connected,
                job,
                capture,
                fingerprint,
                expected.as_ref(),
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
            let mut publication_permit = None;
            if let Some(protection) = protection {
                protection.recheck(cancel).await?;
            }
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
                    let permit = super::runtime::publication_permit(app, &job.id)?;
                    if permit.mode() != live {
                        return Err(ProviderError::new(ErrorKind::Cancelled));
                    }
                    pds(app)?
                        .external_begin_publication(&permit)
                        .map_err(local_error)?;
                    publication_permit = Some(permit);
                    Ok(())
                },
                cancel,
            )
            .await?
            {
                PublicationResult::Confirmed(observed) => {
                    remember_discovery(
                        app,
                        &job.request.connection_id,
                        &completed.snapshot_id,
                        &completed.reference,
                    );
                    let observation = observation_json(&observed.observation)?;
                    let permit = publication_permit
                        .as_ref()
                        .ok_or_else(|| corrupt("missing publication permit"))?;
                    note_sections_published(
                        app,
                        &job.request.connection_id,
                        &published_identity.library_epoch,
                        &completed.sections,
                        &publications,
                    )?;
                    pds(app)?
                        .external_confirm_publication(
                            permit,
                            &job.id,
                            &completed.snapshot_id,
                            &observation,
                        )
                        .map_err(local_error)?;
                    let _ = journal
                        .release_completed_sessions(connected.dependencies.vault.as_ref())
                        .await;
                    super::runtime::record_connection_completion(
                        app,
                        &job.request.connection_id,
                        super::connection_store::CompletionKind::Sync,
                    );
                    Ok(
                        json!({"snapshotId":completed.snapshot_id,"publishedRevision":published_identity.revision.to_string()}),
                    )
                }
                PublicationResult::Conflict(remote) => {
                    drop(_permit);
                    let remote = match remote {
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
                    let record = conflict_record_from_remote(
                        connected,
                        job,
                        local_reference,
                        &remote,
                        cancel,
                    )
                    .await?;
                    let record = preserve_local_conflict(app, job, record)?;
                    drop(journal);
                    ensure_conflict_point(app, connected, job, record, false, protection, cancel).await
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
            preserve_conflict(
                app, connected, job, remote, capture, fingerprint, protection, cancel,
            ).await
        }
        SyncAction::DecisionRequired | SyncAction::RecoveryRequired => {
            Err(ProviderError::new(ErrorKind::PreconditionFailed))
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExternalConflictCursorRequest {
    created_at_ms: i64,
    id: String,
}

fn remember_discovery(
    app: &AppHandle,
    connection_id: &str,
    snapshot_id: &str,
    reference: &super::packaging::RemoteObject,
) {
    let result = super::runtime::root(app).and_then(|root| {
        super::connection_store::ConnectionStore::open(&root)?
            .remember_discovery(connection_id, snapshot_id, reference)
    });
    if result.is_err() {
        crate::nlog!("warn", "External snapshot locator could not be cached");
    }
}

fn conflict_summary(
    record: &ExternalConflictRecord,
    local_available: bool,
    remote_available: bool,
) -> Value {
    json!({
        "id":record.id,
        "connectionId":record.connection_id,
        "detectedAtMs":record.created_at_ms.to_string(),
        "localRevision":record.local.identity.revision.to_string(),
        "remoteRevision":record.remote.logical_revision.to_string(),
        "localAvailable":local_available,
        "remoteAvailable":remote_available,
        "remotePointConfirmed":record.remote_point.is_some(),
        "resolved":record.resolved,
    })
}

#[tauri::command]
pub(crate) fn external_storage_list_conflicts(
    app: AppHandle,
    cursor: Option<ExternalConflictCursorRequest>,
    limit: Option<usize>,
) -> Result<Value> {
    let limit = limit.unwrap_or(50);
    if limit == 0 || limit > 50 {
        return Err(corrupt("invalid conflict page limit"));
    }
    let root = super::runtime::root(&app)?;
    let store = pds(&app)?;
    let cursor = cursor.map(|cursor| external_conflicts::ExternalConflictCursor {
        created_at_ms: cursor.created_at_ms,
        id: cursor.id,
    });
    let page = external_conflicts::external_conflicts_page(
        store.device_store().map_err(local_error)?.connection(),
        cursor.as_ref(),
        limit,
    )
    .map_err(local_error)?;
    let connections = super::connection_store::ConnectionStore::open(&root).ok();
    let conflicts = page
        .conflicts
        .iter()
        .map(|record| {
            let local_available =
                super::capture::registered_capture_roots([&record.local], &root).is_ok();
            let remote_available = connections
                .as_ref()
                .is_some_and(|connections| connections.read(&record.connection_id).is_ok());
            conflict_summary(record, local_available, remote_available)
        })
        .collect::<Vec<_>>();
    let mut result = json!({"conflicts":conflicts});
    if let Some(next) = page.next {
        result["nextCursor"] = json!({"createdAtMs":next.created_at_ms,"id":next.id});
    }
    Ok(result)
}

#[tauri::command]
pub(crate) async fn external_storage_delete_conflict(
    app: AppHandle,
    id: String,
    delete_remote_point: bool,
) -> std::result::Result<Value, crate::native_file_jobs::NativeJobError> {
    if id.is_empty() || id.len() > 1024 || id.contains('\0') {
        return Err(crate::native_file_jobs::NativeJobError::new(
            "invalid-input",
            "External conflict identity is invalid",
        ));
    }
    let state = app.state::<crate::native_file_jobs::NativeFileJobState>();
    let mutation = state.external_conflict_mutation_admission()?;
    let record = {
        let mut store = pds(&app).map_err(|error| {
            crate::native_file_jobs::NativeJobError::new("store-error", error.to_string())
        })?;
        let in_use = state.external_conflict_in_use(&id)?;
        if in_use {
            return Err(crate::native_file_jobs::NativeJobError::new(
                "conflict-source-in-use",
                "External conflict source is in use",
            ));
        }
        let record = external_conflicts::delete_external_conflict(
            store
                .device_store()
                .map_err(|error| {
                    crate::native_file_jobs::NativeJobError::new(
                        "store-error",
                        error.to_string(),
                    )
                })?
                .connection(),
            &id,
        )
        .map_err(|error| {
            crate::native_file_jobs::NativeJobError::new("store-error", error.to_string())
        })?;
        if store.cleanup_deleted_conflict_capture(&record.local).is_err() {
            crate::nlog!(
                "warn",
                "Deleted external conflict capture could not be cleaned immediately"
            );
        }
        record
    };
    drop(mutation);
    drop(state);
    if !delete_remote_point || record.remote_point.is_none() {
        return Ok(json!({"localDeleted":true,"remotePoint":"left-remote"}));
    }
    let remote = async {
        let connected = super::connection_commands::open_connected(&app, &record.connection_id).await?;
        if connected.stored.descriptor.repository_id != record.repository_id {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        control::delete_authenticated_conflict_point(
            &connected,
            &record.id,
            record.remote_point.as_ref().expect("checked conflict point"),
            &Cancellation::default(),
        )
        .await
    }
    .await;
    let remote_point = match remote {
        Ok(control::RemoteConflictPointDeleteOutcome::Deleted) => "deleted",
        Ok(control::RemoteConflictPointDeleteOutcome::NotFound) => "not-found",
        Err(_) => "left-remote",
    };
    Ok(json!({"localDeleted":true,"remotePoint":remote_point}))
}

#[tauri::command]
pub(crate) async fn external_storage_recheck_conflict(
    app: AppHandle,
    id: String,
) -> Result<Value> {
    if id.is_empty() || id.len() > 1024 || id.contains('\0') {
        return Err(corrupt("invalid conflict identity"));
    }
    let root = super::runtime::root(&app)?;
    super::runtime::recheck_preserved_conflict(&app, &id, &Cancellation::default()).await?;
    let record = conflict_record(&app, &id)?
        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
    let local_available =
        super::capture::validate_capture_sources([&record.local], &root).is_ok();
    Ok(conflict_summary(&record, local_available, true))
}

#[cfg(test)]
mod receive_tests {
    use super::*;
    use crate::logical_records::{
        encode_logical_record, encode_logical_record_key, LogicalRecordEnvelope, LogicalRecordLocator,
    };
    use crate::persistent_store::external_storage_state::ReceiveIntent;
    use risunest_external_storage_format::{
        content_identity::hash,
        format::fingerprint,
        section::{local_plugin_entry_key, LocalPluginValue, PluginSpace, SectionEntry, SectionEntryVersion, SectionValue},
        snapshot::{self as wire, CatalogEntryKind},
    };
    use std::{collections::BTreeMap, fs};

    fn bind(store: &mut PersistentStore, snapshot: &str) -> DurableJob {
        let identity = store.external_identity().unwrap();
        let request = serde_json::from_value(json!({
            "connectionId":"connection", "kind":"sync", "reason":"automatic",
            "session":"foreground", "sessionId":"synthetic-session"
        })).unwrap();
        let job = DurableJob::new(request, false, 1, identity.clone());
        store.external_prepare_receive(&ReceiveIntent {
            job_id: &job.id, connection_id: "connection", repository_id: "repository",
            snapshot_id: snapshot, commit_id: snapshot, authenticated_head: "authenticated-head",
            identity: &identity,
        }).unwrap();
        job
    }

    fn fixture() -> (tempfile::TempDir, PersistentStore, DurableJob, super::super::snapshot_restore::PreparedRemoteSnapshot) {
        let directory = tempfile::tempdir().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let epoch = store.external_selection().unwrap().epoch;
        store.external_select(&epoch, &crate::persistent_store::sync_selection::SyncTarget::External("connection".into())).unwrap();
        store.device_store_mut().unwrap().set_section_participating(PdsSection::LocalPlugins, true).unwrap();
        let job = bind(&mut store, "snapshot");
        let staging = directory.path().join("received");
        fs::create_dir(&staging).unwrap();
        let key = encode_logical_record_key(&LogicalRecordLocator::Root).unwrap();
        let record = encode_logical_record(&LogicalRecordEnvelope::Root {
            value: json!({"marker":"remote"}), owner_heads: vec![],
        }).unwrap();
        let path = staging.join("root");
        fs::write(&path, &record.bytes).unwrap();
        let digest = hex::decode(&record.hash).unwrap().try_into().unwrap();
        let library_fingerprint = hex::encode(fingerprint(&library_fingerprint_domain(), &BTreeMap::from([(key.clone(), digest)])));
        let downloaded = super::super::snapshot_restore::PreparedRemoteSnapshot {
            snapshot_id: "snapshot".into(), repository_id: "repository".into(),
            fingerprint: library_fingerprint.clone(), library_fingerprint, logical_revision: 7,
            staging_root: staging, captured_by_device: None, objects: vec![],
            records: vec![super::super::snapshot_restore::PreparedRecord {
                key, content_hash: record.hash, byte_length: record.size, path,
            }],
        };
        (directory, store, job, downloaded)
    }

    fn plugin_section(staging_root: &std::path::Path) -> super::super::sections::CapturedSection {
        let kind = SectionKind::LocalPlugins;
        let key = local_plugin_entry_key("owner", "string", "key").unwrap();
        let entry = SectionEntry::new(kind, key.clone(), SectionValue::LocalPlugin(LocalPluginValue {
            space: PluginSpace::String, value: json!("remote-device-value"),
        }), Some(SectionEntryVersion {
            write_clock: Sequence::from(7u64), writer_id: "remote-writer".into(),
        })).unwrap().encode().unwrap();
        let content_sha256 = hex::encode(hash(&entry));
        let spool = staging_root.join("section-spool");
        fs::create_dir_all(&spool).unwrap();
        let path = spool.join(&content_sha256);
        fs::write(&path, &entry).unwrap();
        super::super::sections::CapturedSection {
            kind, generation: Sequence::from(7u64), gc_floor: Sequence::from(0u64),
            max_write_clock: Sequence::from(7u64),
            content_fingerprint: fingerprint(&kind.fingerprint_domain(), &BTreeMap::from([(key.clone(), hash(&entry))])),
            sources: vec![super::super::sections::SectionSource {
                kind: CatalogEntryKind::SectionEntry, key, content_sha256,
                byte_length: entry.len() as u64, path,
            }],
        }
    }

    fn plugin_section_reference(generation: u64) -> wire::SectionSnapshotRef {
        wire::SectionSnapshotRef {
            kind: SectionKind::LocalPlugins,
            codec: risunest_external_storage_format::section::SECTION_CODEC.into(),
            generation: Sequence::from(generation),
            gc_floor: Sequence::from(0u64),
            max_write_clock: Sequence::from(7u64),
            entries_root: wire::StoredObject {
                header: wire::PublicObjectHeader::new(
                    "repository".into(),
                    "catalog-synthetic".into(),
                    wire::ObjectRole::Catalog,
                    1,
                ).unwrap(),
                locator: wire::WireLocator {
                    connection_identity: "connection".into(),
                    collection: None,
                    object: "catalog-synthetic".into(),
                },
                ciphertext_length: 1,
                ciphertext_sha256: [0; 32],
                plaintext_length: 1,
                plaintext_sha256: [0; 32],
            },
            content_fingerprint: [0; 32],
        }
    }

    fn prepare(store: &mut PersistentStore, job: &DurableJob,
        downloaded: super::super::snapshot_restore::PreparedRemoteSnapshot) -> PreparedReceive
    {
        let participation = receive_participation(store).unwrap();
        let section = plugin_section(&downloaded.staging_root);
        prepare_receive_input(store, job, job.admission_identity.clone(), "authenticated-head".into(),
            downloaded, vec![section], participation, &Cancellation::default()).unwrap()
    }

    fn local_edit(store: &mut PersistentStore, revision: i64) {
        let commit = serde_json::from_value(json!({
            "expectedRevision": revision, "root": {"marker":"newer-local"}
        })).unwrap();
        store.commit(&commit).unwrap();
    }

    fn rows(store: &mut PersistentStore) -> Vec<crate::persistent_store::device_store::sections::SectionRow> {
        store.device_store_mut().unwrap().read_section_rows(PdsSection::LocalPlugins).unwrap()
    }

    #[test]
    fn final_apply_dispatcher_uses_only_prepared_local_handles() {
        let (_directory, mut store, job, downloaded) = fixture();
        let source = downloaded.staging_root.clone();
        let prepared = prepare(&mut store, &job, downloaded);
        let stage = prepared.commit.external_staging_id().to_owned();
        assert_eq!(store.revision().unwrap(), 0);
        assert!(rows(&mut store).is_empty());
        assert_eq!(store.materialize_staging(&stage).unwrap()["marker"], "remote");
        // Final apply must need neither the downloaded library nor section sources.
        fs::remove_dir_all(&source).unwrap();
        // Match the runtime: preparation and activation use separate native handles.
        let mut reopened = store.open_native_job_store().unwrap();
        let state = super::super::job_store::JobCommandState::default();
        state.prepared_receives.lock().unwrap().insert(job.id.clone(), prepared);
        let (cancel, claim) = state.claim(&job).unwrap();
        let current = reopened.external_identity().unwrap();
        cancel.check().unwrap();
        super::super::snapshot_restore::reset_test_read_counts(&source);
        assert_eq!(
            commit_prepared_receive(&mut reopened, &state, &claim, &job, 1, &current)
                .unwrap_err().kind,
            ErrorKind::PreconditionFailed,
        );
        let (snapshot_id, received_revision) = commit_prepared_receive(
            &mut reopened,
            &state,
            &claim,
            &job,
            0,
            &current,
        )
        .unwrap();
        assert_eq!(snapshot_id, "snapshot");
        assert_eq!(received_revision, 1);
        assert_eq!(
            super::super::snapshot_restore::take_test_read_counts(&source),
            (0, 0)
        );
        assert_eq!(store.materialize(None).unwrap()["marker"], "remote");
        assert_eq!(rows(&mut store).len(), 1);
        assert!(store.prepare_replace_commit(&stage, Some(1)).is_err());
    }

    #[test]
    fn failed_receive_reuses_the_original_stage_and_sections_without_downloads() {
        let (directory, mut store, job, downloaded) = fixture();
        let source = downloaded.staging_root.clone();
        let prepared = prepare(&mut store, &job, downloaded);
        let stage = prepared.commit.external_staging_id().to_owned();
        fs::remove_dir_all(&source).unwrap();
        let injector = rusqlite::Connection::open(directory.path().join("persistent").join(crate::persistent_store::DATABASE_FILE)).unwrap();
        injector.execute_batch("CREATE TRIGGER synthetic_receive_failure BEFORE INSERT ON external_storage_bases BEGIN SELECT RAISE(ABORT,'synthetic'); END").unwrap();
        let state = super::super::job_store::JobCommandState::default();
        state.prepared_receives.lock().unwrap().insert(job.id.clone(), prepared);
        let mut first_rows = None;
        for _ in 0..2 {
            let mut native = store.open_native_job_store().unwrap();
            let (_, claim) = state.claim(&job).unwrap();
            let error = commit_prepared_receive(&mut native, &state, &claim, &job, 0, &job.admission_identity).unwrap_err();
            assert_eq!(error.kind, ErrorKind::Transient);
            assert_eq!(native.external_identity().unwrap(), job.admission_identity);
            assert!(completed_receive_result(&native, &job, 0).unwrap().is_none());
            assert_eq!(native.materialize_staging(&stage).unwrap()["marker"], "remote");
            let partial = rows(&mut native);
            assert_eq!(partial.len(), 1);
            if let Some(first) = &first_rows { assert_eq!(&partial, first); }
            else { first_rows = Some(partial); }
            let retained = state.prepared_receives.lock().unwrap();
            assert_eq!(retained.len(), 1);
            assert_eq!(retained[&job.id].commit.external_staging_id(), stage);
            assert_eq!(retained[&job.id].expected, job.admission_identity);
            assert_eq!(retained[&job.id].snapshot_id, "snapshot");
        }
        injector.execute_batch("DROP TRIGGER synthetic_receive_failure").unwrap();
        let mut native = store.open_native_job_store().unwrap();
        let (_, claim) = state.claim(&job).unwrap();
        assert_eq!(commit_prepared_receive(&mut native, &state, &claim, &job, 0, &job.admission_identity).unwrap(),
            ("snapshot".into(), 1));
        assert!(state.prepared_receives.lock().unwrap().is_empty());
        assert!(!source.exists());
        assert_eq!(rows(&mut native), first_rows.unwrap());
        assert_eq!(native.materialize(None).unwrap()["marker"], "remote");
        assert!(native.prepare_replace_commit(&stage, Some(1)).is_err());
        assert_eq!(completed_receive_result(&native, &job, 0).unwrap().unwrap()["receivedRevision"], "1");
    }

    #[test]
    fn retry_after_a_failed_receive_still_rejects_a_changed_library() {
        let (directory, mut store, job, downloaded) = fixture();
        let prepared = prepare(&mut store, &job, downloaded);
        let stage = prepared.commit.external_staging_id().to_owned();
        let injector = rusqlite::Connection::open(directory.path().join("persistent").join(crate::persistent_store::DATABASE_FILE)).unwrap();
        injector.execute_batch("CREATE TRIGGER synthetic_receive_failure BEFORE INSERT ON external_storage_bases BEGIN SELECT RAISE(ABORT,'synthetic'); END").unwrap();
        let state = super::super::job_store::JobCommandState::default();
        state.prepared_receives.lock().unwrap().insert(job.id.clone(), prepared);
        let (_, claim) = state.claim(&job).unwrap();
        assert_eq!(commit_prepared_receive(&mut store, &state, &claim, &job, 0, &job.admission_identity).unwrap_err().kind,
            ErrorKind::Transient);
        let partial = rows(&mut store);
        assert_eq!(partial.len(), 1);
        injector.execute_batch("DROP TRIGGER synthetic_receive_failure").unwrap();
        local_edit(&mut store, 0);
        let current = store.external_identity().unwrap();
        assert_eq!(commit_prepared_receive(&mut store, &state, &claim, &job, 0, &current).unwrap_err().kind,
            ErrorKind::PreconditionFailed);
        assert!(state.prepared_receives.lock().unwrap().is_empty());
        assert!(store.prepare_replace_commit(&stage, Some(1)).is_err());
        assert_eq!(store.materialize(None).unwrap()["marker"], "newer-local");
        assert!(completed_receive_result(&store, &job, 0).unwrap().is_none());
        // Device-section progress is preserved honestly; this is not rollback.
        assert_eq!(rows(&mut store), partial);
    }

    #[test]
    fn completed_receive_is_stable_after_later_local_writes_and_another_receive() {
        let (_directory, mut store, job, downloaded) = fixture();
        let second_input = downloaded.clone();
        let prepared = prepare(&mut store, &job, downloaded);
        let expected = job.admission_identity.clone();
        activate_prepared_receive(&mut store, &prepared, &expected).unwrap();
        // The auxiliary summary deliberately remains queued, as after a failed cache write.
        assert_eq!(job.summary["state"], "queued");
        local_edit(&mut store, 1);
        let second = bind(&mut store, "second-snapshot");
        let mut second_input = second_input;
        second_input.snapshot_id = "second-snapshot".into();
        let prepared = prepare(&mut store, &second, second_input);
        activate_prepared_receive(&mut store, &prepared, &second.admission_identity).unwrap();
        assert_eq!(store.external_base("connection").unwrap().unwrap().snapshot_id, "second-snapshot");
        let result = completed_receive_result(&store, &job, 0).unwrap().unwrap();
        assert_eq!(result["receivedRevision"], "1");
        assert_eq!(result["snapshotId"], "snapshot");
        assert!(completed_receive_result(&store, &job, 1).is_err());
        assert_eq!(store.revision().unwrap(), 3);
    }

    #[test]
    fn failed_auxiliary_summary_write_does_not_erase_or_repeat_a_committed_receive() {
        let (directory, mut store, mut job, downloaded) = fixture();
        let prepared = prepare(&mut store, &job, downloaded);
        job.receive_staging_id = Some(prepared.commit.external_staging_id().to_owned());
        let cache = JobStore::open(directory.path()).unwrap();
        cache.put(&job).unwrap();
        let injector = rusqlite::Connection::open(directory.path().join("external-jobs.sqlite")).unwrap();
        injector.execute_batch(
            "CREATE TRIGGER synthetic_summary_failure BEFORE UPDATE ON external_requests
             BEGIN SELECT RAISE(ABORT,'synthetic summary failure'); END;"
        ).unwrap();
        activate_prepared_receive(&mut store, &prepared, &job.admission_identity).unwrap();
        job.summary["state"] = json!("succeeded");
        job.receive_staging_id = None;
        assert!(cache.put(&job).is_err());
        let stale = cache.read(&job.id).unwrap();
        assert_eq!(stale.summary["state"], "queued");
        assert!(stale.receive_staging_id.is_some());
        for _ in 0..3 {
            assert_eq!(completed_receive_result(&store, &stale, 0).unwrap().unwrap()["receivedRevision"], "1");
        }
        assert_eq!(store.revision().unwrap(), 1);
        assert_eq!(rows(&mut store).len(), 1);
    }

    #[test]
    fn selection_changes_invalidate_ready_receives_before_device_rows_are_changed() {
        let (_directory, mut store, job, downloaded) = fixture();
        let prepared = prepare(&mut store, &job, downloaded);
        let epoch = store.external_selection().unwrap().epoch;
        store.external_select(&epoch, &crate::persistent_store::sync_selection::SyncTarget::None).unwrap();
        assert_eq!(store.external_job(&job.id).unwrap().unwrap().phase, "stale");
        let current = store.external_identity().unwrap();
        assert!(activate_prepared_receive(&mut store, &prepared, &current).is_err());
        assert_eq!(store.revision().unwrap(), 0);
        assert!(rows(&mut store).is_empty());
    }

    #[test]
    fn prepared_handle_is_claimed_once_and_wrong_requests_do_not_consume_it() {
        let (_directory, mut store, job, downloaded) = fixture();
        let prepared = prepare(&mut store, &job, downloaded);
        let state = super::super::job_store::JobCommandState::default();
        state.prepared_receives.lock().unwrap().insert(job.id.clone(), prepared);
        let other_state = super::super::job_store::JobCommandState::default();
        let (_, unrelated) = other_state.claim(&job).unwrap();
        assert!(take_prepared_receive(&state, &unrelated, &job, 0).is_err());
        let (_, claim) = state.claim(&job).unwrap();
        assert!(take_prepared_receive(&state, &claim, &job, 1).is_err());
        assert_eq!(state.prepared_receives.lock().unwrap().len(), 1);
        let prepared = take_prepared_receive(&state, &claim, &job, 0).unwrap();
        assert!(take_prepared_receive(&state, &claim, &job, 0).is_err());
        activate_prepared_receive(&mut store, &prepared, &job.admission_identity).unwrap();
        drop(claim);
        assert_eq!(completed_receive_result(&store, &job, 0).unwrap().unwrap()["receivedRevision"], "1");
        assert_eq!(store.revision().unwrap(), 1);
    }

    #[test]
    fn cancellation_does_not_consume_an_apply_handle_or_release_its_worker() {
        let (_directory, mut store, job, downloaded) = fixture();
        let prepared = prepare(&mut store, &job, downloaded);
        let state = super::super::job_store::JobCommandState::default();
        state.prepared_receives.lock().unwrap().insert(job.id.clone(), prepared);
        let (cancel, claim) = state.claim(&job).unwrap();
        cancel.cancel();
        assert_eq!(take_prepared_receive(&state, &claim, &job, 0).err().unwrap().kind, ErrorKind::Cancelled);
        assert_eq!(state.prepared_receives.lock().unwrap().len(), 1);
        assert!(state.claim(&job).is_err());
        assert_eq!(store.revision().unwrap(), 0);
        drop(claim);
        assert!(state.claim(&job).is_ok());
    }

    #[test]
    fn each_identity_component_is_checked_before_device_rows_or_library_activation() {
        for field in ["store", "library", "generation", "selection", "revision"] {
            let (_directory, mut store, job, downloaded) = fixture();
            let prepared = prepare(&mut store, &job, downloaded);
            let stage = prepared.commit.external_staging_id().to_owned();
            let mut current = job.admission_identity.clone();
            match field {
                "store" => current.store_id.push('x'),
                "library" => current.library_epoch.push('x'),
                "generation" => current.generation.push('x'),
                "selection" => current.selection_epoch.push('x'),
                "revision" => current.revision += 1,
                _ => unreachable!(),
            }
            assert_eq!(activate_prepared_receive(&mut store, &prepared, &current).err().unwrap().kind,
                ErrorKind::PreconditionFailed, "{field}");
            assert!(rows(&mut store).is_empty(), "{field}");
            assert_eq!(store.revision().unwrap(), 0, "{field}");
            assert!(store.prepare_replace_commit(&stage, Some(0)).is_err(), "{field}");
        }
    }

    #[test]
    fn a_local_edit_during_preparation_or_before_apply_is_never_rebased() {
        for before_stage in [true, false] {
            let (_directory, mut store, job, downloaded) = fixture();
            if before_stage {
                local_edit(&mut store, 0);
                let participation = receive_participation(&mut store).unwrap();
                let section = plugin_section(&downloaded.staging_root);
                assert!(prepare_receive_input(&mut store, &job, job.admission_identity.clone(),
                    "authenticated-head".into(), downloaded, vec![section], participation,
                    &Cancellation::default()).is_err());
            } else {
                let prepared = prepare(&mut store, &job, downloaded);
                local_edit(&mut store, 0);
                let current = store.external_identity().unwrap();
                assert!(activate_prepared_receive(&mut store, &prepared, &current).is_err());
            }
            assert_eq!(store.revision().unwrap(), 1);
            assert_eq!(store.materialize(None).unwrap()["marker"], "newer-local");
            assert!(rows(&mut store).is_empty());
            assert!(store.external_receive_completion(&job.id, "connection").unwrap().is_none());
        }
    }

    #[test]
    fn participation_generation_changes_reject_even_when_the_flag_returns_to_true() {
        let (_directory, mut store, job, downloaded) = fixture();
        let prepared = prepare(&mut store, &job, downloaded);
        let device = store.device_store_mut().unwrap();
        device.set_section_participating(PdsSection::LocalPlugins, false).unwrap();
        device.set_section_participating(PdsSection::LocalPlugins, true).unwrap();
        assert!(activate_prepared_receive(&mut store, &prepared, &job.admission_identity).is_err());
        assert!(rows(&mut store).is_empty());
        assert_eq!(store.revision().unwrap(), 0);
    }

    #[test]
    fn publication_ack_skips_carried_sections() {
        let (_directory, mut store, _job, _downloaded) = fixture();
        let sections = BTreeMap::from([(
            SectionKind::LocalPlugins.id().to_owned(),
            plugin_section_reference(7),
        )]);
        note_sections_published_in_store(&mut store, "connection", "library", &sections, &[])
            .unwrap();
        assert!(store.device_store_mut().unwrap()
            .read_section_cursor("connection", "library", PdsSection::LocalPlugins)
            .unwrap().is_none());
    }

    #[test]
    fn corrupt_sections_or_snapshot_bindings_never_create_an_applicable_receive() {
        for case in ["section", "snapshot", "repository", "record"] {
            let (_directory, mut store, job, mut downloaded) = fixture();
            let participation = receive_participation(&mut store).unwrap();
            let section = plugin_section(&downloaded.staging_root);
            match case {
                "section" => fs::write(&section.sources[0].path, b"corrupt").unwrap(),
                "snapshot" => downloaded.snapshot_id = "other-snapshot".into(),
                "repository" => downloaded.repository_id = "other-repository".into(),
                "record" => fs::write(&downloaded.records[0].path, b"corrupt").unwrap(),
                _ => unreachable!(),
            }
            assert!(prepare_receive_input(&mut store, &job, job.admission_identity.clone(),
                "authenticated-head".into(), downloaded, vec![section], participation,
                &Cancellation::default()).is_err(), "{case}");
            assert_eq!(store.revision().unwrap(), 0, "{case}");
            assert!(rows(&mut store).is_empty(), "{case}");
        }
    }

    #[test]
    fn interrupted_section_progress_is_idempotent_after_worker_repreparation() {
        let (directory, mut store, job, downloaded) = fixture();
        let prepared = prepare(&mut store, &job, downloaded.clone());
        for section in &prepared.sections {
            super::super::sections::apply_prepared_section(&mut store, section).unwrap();
        }
        let first = rows(&mut store);
        assert_eq!(first.len(), 1);
        assert_eq!(store.revision().unwrap(), 0);
        store.replace_abort(prepared.commit.external_staging_id()).unwrap();
        drop(prepared);
        drop(store);
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let state = super::super::job_store::JobCommandState::default();
        let (_, claim) = state.claim(&job).unwrap();
        assert!(take_prepared_receive(&state, &claim, &job, 0).is_err());
        let prepared = prepare(&mut store, &job, downloaded);
        activate_prepared_receive(&mut store, &prepared, &job.admission_identity).unwrap();
        assert_eq!(rows(&mut store), first);
        assert_eq!(store.revision().unwrap(), 1);
    }

    #[test]
    fn receive_head_checks_content_without_binding_to_an_old_provider_token() {
        let original = HeadObservation {
            commit_id: "received-commit".into(),
            authenticated_body_hash: "12".repeat(32),
            version: Some(VersionToken("before".into())),
        };
        let encoded = observation_json(&original).unwrap();
        let mut current = original.clone();
        for version in [Some(VersionToken("after".into())), None] {
            current.version = version;
            assert!(require_received_head(&encoded, Some(&current)).is_ok());
        }
        current.commit_id = "different-commit".into();
        assert_eq!(require_received_head(&encoded, Some(&current)).unwrap_err().kind,
            ErrorKind::PreconditionFailed);
        current.commit_id = original.commit_id;
        current.authenticated_body_hash = "34".repeat(32);
        assert_eq!(require_received_head(&encoded, Some(&current)).unwrap_err().kind,
            ErrorKind::PreconditionFailed);
        assert_eq!(require_received_head(&encoded, None).unwrap_err().kind,
            ErrorKind::PreconditionFailed);
        assert_eq!(require_received_head("invalid", Some(&current)).unwrap_err().kind,
            ErrorKind::Corrupt);
    }

    #[test]
    fn apply_requests_accept_only_canonical_revisions_and_no_native_stage_fields() {
        for revision in ["-1", "+1", "01", "1.0", " 1", "9223372036854775808"] {
            assert!(receive_revision(&ApplyReceivedRequest {
                job_id: "job".into(), expected_revision: revision.into(),
            }).is_err());
        }
        assert!(serde_json::from_value::<ApplyReceivedRequest>(json!({
            "jobId":"job", "expectedRevision":"0", "stagingId":"staging-injected"
        })).is_err());
        assert_eq!(receive_revision(&ApplyReceivedRequest {
            job_id: "job".into(), expected_revision: "0".into(),
        }).unwrap(), 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::{
        contract::{ObjectReceipt, ObjectRole, RemoteLocator},
        control::HeadDocument,
        fake,
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

    async fn observe(
        provider: &fake::FakeProvider,
        cancel: &Cancellation,
    ) -> Result<Option<ObservedHead>> {
        observe_unknown_publication(
            provider,
            &fake::repository(),
            &descriptor(),
            &[7; 32],
            "intended-commit",
            "snapshot-intended",
            cancel,
        )
        .await
    }

    #[test]
    fn publication_unknown_observation_is_bounded_cancelable_and_read_only() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let missing = fake::FakeProvider::new(false);
            assert!(observe(&missing, &Cancellation::default()).await.unwrap().is_none());
            assert_eq!(missing.read_attempts("head"), 2);
            assert!(!missing.holds("head"));
            assert!(missing.uploaded_ids().is_empty());

            let transient = fake::FakeProvider::new(false);
            transient.fail_read("head", ErrorKind::Transient);
            assert!(observe(&transient, &Cancellation::default()).await.unwrap().is_none());
            assert_eq!(transient.read_attempts("head"), 2);
            assert!(!transient.holds("head"));
            assert!(transient.uploaded_ids().is_empty());

            let cancelled = fake::FakeProvider::new(false);
            let cancellation = Cancellation::default();
            cancelled.cancel_after_read("head", 1, &cancellation);
            assert_eq!(
                observe(&cancelled, &cancellation).await.unwrap_err().kind,
                ErrorKind::Cancelled,
            );
            assert_eq!(cancelled.read_attempts("head"), 1);
            assert!(!cancelled.holds("head"));
            assert!(cancelled.uploaded_ids().is_empty());

            for kind in [ErrorKind::Unauthorized, ErrorKind::ReauthRequired, ErrorKind::Corrupt] {
                let permanent = fake::FakeProvider::new(false);
                permanent.fail_read("head", kind);
                assert_eq!(
                    observe(&permanent, &Cancellation::default()).await.unwrap_err().kind,
                    kind,
                );
                assert_eq!(permanent.read_attempts("head"), 1, "{kind:?}");
                assert!(!permanent.holds("head"), "{kind:?}");
                assert!(permanent.uploaded_ids().is_empty(), "{kind:?}");
            }
        });
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
            SyncAction::PreserveConflict { remote: new }
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
