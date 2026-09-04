use super::device_registry::{
    completion_receipt_id, finalize_incoming_completion_delivery,
    incoming_completed_operation_bytes_for_lane, incoming_completion_is_durable,
    incoming_source_by_id, prepare_incoming_completion_delivery,
    record_incoming_completed_operation_once_for_lane, snapshot_incoming_completion_delivery,
    CompletionDeliveryPrepareStatus, CompletionLane, IncomingSource, PendingCompletionDelivery,
};
use super::lan::{
    authenticated_peer_hello_with_capabilities, deliver_peer_completion,
    prepare_peer_logical_completion, PeerCompletionCapability, PeerCompletionDelivery,
};
use super::PeerSyncError;
use crate::{
    asset_repository::job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob},
    persistent_store::SyncGenerationIdentity,
    persistent_store::{PersistentStore, PRODUCT_LOGICAL_LIBRARY_ID},
    trust_boundary::{is_link_like, is_lower_hex_256, sync_directory},
};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use std::collections::BTreeSet;
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// Stable code the interface maps to its own wording; never shown as native
/// text. The recovery path that produces it lands with retained completion
/// recovery.
#[cfg(any(desktop, target_os = "android"))]
pub(crate) const DELTA_COMPLETION_AMBIGUOUS: &str = "peer-delta-completion-ambiguous";
const DELTA_COMPLETION_SCHEMA: &str = "risunest.peer-delta-completion/v1";
const DELTA_COMPLETION_FILE: &str = "completion-operation.json";
const DELTA_COMPLETION_TOMBSTONE: &str = ".completion-operation.deleted";
const MAX_DELTA_COMPLETION_BYTES: u64 = 16 * 1024;
const P4_DELTA_TARGET_JOB_PREFIX: &str = "p4-delta-target-";

fn journal_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[cfg(test)]
fn journal_store_failures() -> &'static Mutex<BTreeSet<PathBuf>> {
    static FAILURES: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
    FAILURES.get_or_init(|| Mutex::new(BTreeSet::new()))
}

#[cfg(test)]
pub(crate) fn fail_next_delta_completion_store_after_replace(app_root: &Path) {
    journal_store_failures()
        .lock()
        .expect("delta completion test failure lock")
        .insert(PeerDeltaCompletionJournal::new(app_root).path());
}

#[cfg(test)]
fn fail_store_after_replace_if_requested(path: &Path) -> Result<(), PeerSyncError> {
    if journal_store_failures()
        .lock()
        .map_err(|_| {
            PeerSyncError::Storage("delta completion test failure lock failed".to_owned())
        })?
        .remove(path)
    {
        return Err(PeerSyncError::Storage(
            "injected journal publication failure".to_owned(),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DeltaCompletionMode {
    CompletionV1,
    UnsupportedV2,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DeltaCompletionContext {
    pub(crate) operation_id: String,
    pub(crate) source_device_id: String,
    pub(crate) manifest_id: String,
    pub(crate) mode: DeltaCompletionMode,
    pub(crate) pre_revision: i64,
    pub(crate) pre_common_base: Option<SyncGenerationIdentity>,
    pub(crate) post_revision: i64,
    pub(crate) post_common_base: SyncGenerationIdentity,
    pub(crate) transferred_objects: u64,
    pub(crate) transferred_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "phase", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum PeerDeltaDurableCompletion {
    ActivationIntent { context: DeltaCompletionContext },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeltaCommitWitness {
    Committed,
    Uncommitted,
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveredDeltaCompletion {
    pub(crate) context: DeltaCompletionContext,
    pub(crate) useful_bytes: u64,
}

pub(crate) trait DeltaCompletionTransport {
    fn prepare(
        &mut self,
        source: &IncomingSource,
        context: &DeltaCompletionContext,
    ) -> Result<u64, PeerSyncError>;

    fn deliver(
        &mut self,
        source: &IncomingSource,
        delivery: &PendingCompletionDelivery,
    ) -> Result<(), PeerSyncError>;
}

pub(crate) struct LanDeltaCompletionTransport;

impl DeltaCompletionTransport for LanDeltaCompletionTransport {
    fn prepare(
        &mut self,
        source: &IncomingSource,
        context: &DeltaCompletionContext,
    ) -> Result<u64, PeerSyncError> {
        validate_v1_source(source)?;
        prepare_peer_logical_completion(
            &source.endpoint,
            &source.bearer,
            CompletionLane::Delta,
            &context.operation_id,
            &context.manifest_id,
        )
    }

    fn deliver(
        &mut self,
        source: &IncomingSource,
        delivery: &PendingCompletionDelivery,
    ) -> Result<(), PeerSyncError> {
        validate_v1_source(source)?;
        match deliver_peer_completion(
            &source.endpoint,
            &source.bearer,
            PeerCompletionCapability::V1,
            CompletionLane::Delta,
            &delivery.completion_lease_id,
            &delivery.manifest_id,
            delivery.useful_bytes,
        )? {
            PeerCompletionDelivery::Delivered => Ok(()),
            PeerCompletionDelivery::Unsupported => {
                invalid("delta completion capability changed during delivery")
            }
        }
    }
}

fn validate_v1_source(source: &IncomingSource) -> Result<(), PeerSyncError> {
    let observed = authenticated_peer_hello_with_capabilities(&source.endpoint, &source.bearer)?;
    if observed.hello.device_id != source.device_id
        || !observed.hello.permissions.allows_read()
        || observed.completion != PeerCompletionCapability::V1
    {
        return invalid("delta completion source identity or capability changed");
    }
    Ok(())
}

impl DeltaCompletionContext {
    pub(crate) fn classify_witness(
        &self,
        revision: i64,
        common_base: Option<&SyncGenerationIdentity>,
    ) -> DeltaCommitWitness {
        if revision >= self.post_revision && common_base == Some(&self.post_common_base) {
            DeltaCommitWitness::Committed
        } else if revision >= self.pre_revision && common_base == self.pre_common_base.as_ref() {
            DeltaCommitWitness::Uncommitted
        } else {
            DeltaCommitWitness::Unknown
        }
    }

    fn validate(&self) -> Result<(), PeerSyncError> {
        validate_uuid_v4(&self.operation_id, "delta completion operation")?;
        validate_uuid(&self.source_device_id, "delta completion source")?;
        if !is_lower_hex_256(&self.manifest_id)
            || self.manifest_id != self.post_common_base.manifest_hash
            || self.pre_revision < 0
            || self.post_revision < self.pre_revision
            || self.post_revision > self.pre_revision.saturating_add(1)
            || !valid_generation(&self.post_common_base)
            || self
                .pre_common_base
                .as_ref()
                .is_some_and(|identity| !valid_generation(identity))
        {
            return invalid("delta completion context is invalid");
        }
        Ok(())
    }
}

impl PeerDeltaDurableCompletion {
    pub(crate) fn context(&self) -> &DeltaCompletionContext {
        match self {
            Self::ActivationIntent { context } => context,
        }
    }

    fn validate(&self) -> Result<(), PeerSyncError> {
        self.context().validate()
    }
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DeltaCompletionFile {
    schema: String,
    operation: PeerDeltaDurableCompletion,
}

pub(crate) struct PeerDeltaCompletionJournal {
    root: PathBuf,
}

impl PeerDeltaCompletionJournal {
    pub(crate) fn new(app_root: &Path) -> Self {
        Self {
            root: app_root.join("peer-delta"),
        }
    }

    pub(crate) fn load(&self) -> Result<Option<PeerDeltaDurableCompletion>, PeerSyncError> {
        let _guard = journal_lock().lock().map_err(|_| {
            PeerSyncError::Storage("delta completion journal lock failed".to_owned())
        })?;
        self.load_locked()
    }

    /// `source_device_id` of `None` matches any retained operation.
    pub(crate) fn references_source(
        &self,
        source_device_id: Option<&str>,
    ) -> Result<bool, PeerSyncError> {
        Ok(self.load()?.is_some_and(|operation| {
            source_device_id.is_none_or(|wanted| operation.context().source_device_id == wanted)
        }))
    }

    pub(crate) fn store_activation_intent(
        &self,
        context: &DeltaCompletionContext,
    ) -> Result<(), PeerSyncError> {
        context.validate()?;
        let _guard = journal_lock().lock().map_err(|_| {
            PeerSyncError::Storage("delta completion journal lock failed".to_owned())
        })?;
        if let Some(existing) = self.load_locked()? {
            return if existing.context() == context {
                Ok(())
            } else {
                invalid("another delta completion operation is retained")
            };
        }
        self.store_locked(&PeerDeltaDurableCompletion::ActivationIntent {
            context: context.clone(),
        })
    }

    pub(crate) fn remove_exact(
        &self,
        context: &DeltaCompletionContext,
    ) -> Result<(), PeerSyncError> {
        let _guard = journal_lock().lock().map_err(|_| {
            PeerSyncError::Storage("delta completion journal lock failed".to_owned())
        })?;
        let Some(existing) = self.load_locked()? else {
            self.finish_missing_delete()?;
            return Ok(());
        };
        if existing.context() != context {
            return invalid("another delta completion operation is retained");
        }
        let path = self.path();
        self.delete_canonical(&path)
    }

    fn load_locked(&self) -> Result<Option<PeerDeltaDurableCompletion>, PeerSyncError> {
        match fs::symlink_metadata(&self.root) {
            Ok(metadata) if metadata.is_dir() && !is_link_like(&metadata) => {}
            Ok(_) => return invalid("delta completion directory is invalid"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        }
        let path = self.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.finish_missing_delete()?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        if !metadata.is_file()
            || is_link_like(&metadata)
            || metadata.len() > MAX_DELTA_COMPLETION_BYTES
        {
            return invalid("delta completion journal is invalid");
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(path)?
            .take(MAX_DELTA_COMPLETION_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_DELTA_COMPLETION_BYTES {
            return invalid("delta completion journal exceeds its bound");
        }
        let file: DeltaCompletionFile = serde_json::from_slice(&bytes)
            .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        if file.schema != DELTA_COMPLETION_SCHEMA {
            return invalid("delta completion journal schema is unsupported");
        }
        file.operation.validate()?;
        Ok(Some(file.operation))
    }

    fn store_locked(&self, operation: &PeerDeltaDurableCompletion) -> Result<(), PeerSyncError> {
        operation.validate()?;
        let bytes = serde_json::to_vec(&DeltaCompletionFile {
            schema: DELTA_COMPLETION_SCHEMA.to_owned(),
            operation: operation.clone(),
        })
        .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
        if bytes.len() as u64 > MAX_DELTA_COMPLETION_BYTES {
            return invalid("delta completion journal exceeds its bound");
        }
        self.ensure_root()?;
        let path = self.path();
        if let Ok(metadata) = fs::symlink_metadata(&path) {
            if !metadata.is_file() || is_link_like(&metadata) {
                return invalid("delta completion journal is invalid");
            }
        }
        let temporary = self
            .root
            .join(format!(".completion-{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = create_owner_only(&temporary)?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.sync_all()?;
            drop(file);
            replace_file_atomic(&temporary, &path)?;
            #[cfg(test)]
            fail_store_after_replace_if_requested(&path)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    fn ensure_root(&self) -> Result<(), PeerSyncError> {
        match fs::symlink_metadata(&self.root) {
            Ok(metadata) if metadata.is_dir() && !is_link_like(&metadata) => {}
            Ok(_) => return invalid("delta completion directory is invalid"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    fs::DirBuilder::new().mode(0o700).create(&self.root)?;
                }
                #[cfg(not(unix))]
                fs::create_dir(&self.root)?;
                let parent = self.root.parent().ok_or_else(|| {
                    PeerSyncError::Storage("delta completion directory has no parent".to_owned())
                })?;
                let _ = sync_directory(parent)?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn path(&self) -> PathBuf {
        self.root.join(DELTA_COMPLETION_FILE)
    }

    fn tombstone_path(&self) -> PathBuf {
        self.root.join(DELTA_COMPLETION_TOMBSTONE)
    }

    #[cfg(windows)]
    fn delete_canonical(&self, path: &Path) -> Result<(), PeerSyncError> {
        let tombstone = self.tombstone_path();
        validate_optional_regular_file(&tombstone)?;
        replace_file_atomic(path, &tombstone)?;
        self.finish_missing_delete()
    }

    #[cfg(not(windows))]
    fn delete_canonical(&self, path: &Path) -> Result<(), PeerSyncError> {
        unlink_canonical_with_sync(path, || {
            let _ = sync_directory(&self.root)?;
            Ok(())
        })
    }

    fn finish_missing_delete(&self) -> Result<(), PeerSyncError> {
        match fs::symlink_metadata(&self.root) {
            Ok(metadata) if metadata.is_dir() && !is_link_like(&metadata) => {}
            Ok(_) => return invalid("delta completion directory is invalid"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        let tombstone = self.tombstone_path();
        validate_optional_regular_file(&tombstone)?;
        match fs::remove_file(&tombstone) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        #[cfg(not(windows))]
        {
            let _ = sync_directory(&self.root)?;
        }
        Ok(())
    }
}

#[cfg(any(not(windows), test))]
pub(super) fn unlink_canonical_with_sync(
    path: &Path,
    sync_root: impl FnOnce() -> Result<(), PeerSyncError>,
) -> Result<(), PeerSyncError> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    sync_root()
}

fn validate_optional_regular_file(path: &Path) -> Result<(), PeerSyncError> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_file()
                && !is_link_like(&metadata)
                && metadata.len() <= MAX_DELTA_COMPLETION_BYTES =>
        {
            Ok(())
        }
        Ok(_) => invalid("delta completion tombstone is invalid"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn complete_delta_accounting(
    app_root: &Path,
    transport: &mut impl DeltaCompletionTransport,
) -> Result<u64, PeerSyncError> {
    let journal = PeerDeltaCompletionJournal::new(app_root);
    let operation = journal.load()?.ok_or_else(|| {
        PeerSyncError::Validation("delta completion journal is missing".to_owned())
    })?;
    let context = operation.context().clone();
    let receipt_id = completion_receipt_id(
        CompletionLane::Delta.as_str(),
        &context.operation_id,
        &context.manifest_id,
    );

    let completed_bytes = incoming_completed_operation_bytes_for_lane(
        app_root,
        &context.source_device_id,
        CompletionLane::Delta,
        &receipt_id,
    )?;

    match context.mode {
        DeltaCompletionMode::CompletionV1 => {
            if let Some(useful_bytes) = completed_bytes {
                return Ok(useful_bytes);
            }
            let delivery = match snapshot_incoming_completion_delivery(
                app_root,
                &context.source_device_id,
                CompletionLane::Delta,
            )? {
                Some(snapshot) => {
                    validate_delta_delivery(&snapshot.delivery, &context, &receipt_id)?;
                    snapshot.delivery
                }
                None => {
                    let source = incoming_source_by_id(app_root, &context.source_device_id)?
                        .ok_or_else(|| {
                            PeerSyncError::Validation(
                                "registered incoming source is missing".to_owned(),
                            )
                        })?;
                    let useful_bytes = transport.prepare(&source, &context)?;
                    let delivery = delta_delivery(&context, receipt_id.clone(), useful_bytes);
                    match prepare_incoming_completion_delivery(app_root, delivery.clone())? {
                        CompletionDeliveryPrepareStatus::Pending => delivery,
                        CompletionDeliveryPrepareStatus::AlreadyDurable => {
                            return incoming_completed_operation_bytes_for_lane(
                                app_root,
                                &context.source_device_id,
                                CompletionLane::Delta,
                                &receipt_id,
                            )?
                            .filter(|completed| *completed == useful_bytes)
                            .ok_or_else(|| {
                                PeerSyncError::Validation(
                                    "delta completion receipt is not exact".to_owned(),
                                )
                            });
                        }
                    }
                }
            };
            let snapshot = snapshot_incoming_completion_delivery(
                app_root,
                &context.source_device_id,
                CompletionLane::Delta,
            )?
            .ok_or_else(|| {
                PeerSyncError::Validation("pending delta completion delivery is missing".to_owned())
            })?;
            if snapshot.delivery != delivery {
                return invalid("pending delta completion delivery does not match");
            }
            transport.deliver(&snapshot.source, &snapshot.delivery)?;
            finalize_incoming_completion_delivery(app_root, &delivery)?;
            if !incoming_completion_is_durable(app_root, &delivery)? {
                return invalid("delta completion delivery is not durable");
            }
            Ok(delivery.useful_bytes)
        }
        DeltaCompletionMode::UnsupportedV2 => {
            let useful_bytes = context.transferred_bytes;
            if let Some(completed_bytes) = completed_bytes {
                return if completed_bytes == useful_bytes {
                    Ok(useful_bytes)
                } else {
                    invalid("delta completion receipt byte count conflicts")
                };
            }
            record_incoming_completed_operation_once_for_lane(
                app_root,
                &context.source_device_id,
                CompletionLane::Delta,
                &receipt_id,
                useful_bytes,
            )?;
            if incoming_completed_operation_bytes_for_lane(
                app_root,
                &context.source_device_id,
                CompletionLane::Delta,
                &receipt_id,
            )? != Some(useful_bytes)
            {
                return invalid("delta completion receipt is not durable");
            }
            Ok(useful_bytes)
        }
    }
}

pub(crate) fn recover_delta_completion(
    store: &mut PersistentStore,
    app_root: &Path,
    transport: &mut impl DeltaCompletionTransport,
) -> Result<Option<RecoveredDeltaCompletion>, PeerSyncError> {
    let journal = PeerDeltaCompletionJournal::new(app_root);
    let Some(operation) = journal.load()? else {
        return Ok(None);
    };
    let context = operation.context().clone();
    let revision = store
        .revision()
        .map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    let common_base = store
        .sync_device_common_base_identity(PRODUCT_LOGICAL_LIBRARY_ID, &context.source_device_id)
        .map_err(|error| PeerSyncError::Storage(error.to_string()))?;

    match context.classify_witness(revision, common_base.as_ref()) {
        DeltaCommitWitness::Unknown => invalid("delta completion commit witness is ambiguous"),
        DeltaCommitWitness::Uncommitted => {
            let job = open_uncommitted_job(app_root, &context.operation_id)?;
            journal.remove_exact(&context)?;
            if let Some(mut job) = job {
                job.release(CasReleaseOutcome::Aborted)?;
            }
            Ok(None)
        }
        DeltaCommitWitness::Committed => {
            let useful_bytes = complete_delta_accounting(app_root, transport)?;
            journal.remove_exact(&context)?;
            Ok(Some(RecoveredDeltaCompletion {
                context,
                useful_bytes,
            }))
        }
    }
}

fn open_uncommitted_job(
    app_root: &Path,
    operation_id: &str,
) -> Result<Option<DurableCasJob>, PeerSyncError> {
    let job_id = format!("{P4_DELTA_TARGET_JOB_PREFIX}{operation_id}");
    let job = match DurableCasJob::open(app_root, &job_id) {
        Ok(job) => job,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if job.kind() != CasJobKind::LogicalDeltaTarget {
        return invalid("delta completion CAS job belongs to another owner");
    }
    Ok(Some(job))
}

/// `source_device_id` of `None` asks the same question about any registered source.
pub(crate) fn registered_delta_source_is_active(
    app_root: &Path,
    source_device_id: Option<&str>,
) -> Result<bool, PeerSyncError> {
    if let Some(source_device_id) = source_device_id {
        validate_uuid(source_device_id, "delta completion source")?;
    }
    PeerDeltaCompletionJournal::new(app_root).references_source(source_device_id)
}

fn delta_delivery(
    context: &DeltaCompletionContext,
    receipt_id: String,
    useful_bytes: u64,
) -> PendingCompletionDelivery {
    PendingCompletionDelivery {
        source_device_id: context.source_device_id.clone(),
        lane: CompletionLane::Delta.as_str().to_owned(),
        completion_lease_id: context.operation_id.clone(),
        manifest_id: context.manifest_id.clone(),
        useful_bytes,
        receipt_id,
    }
}

fn validate_delta_delivery(
    delivery: &PendingCompletionDelivery,
    context: &DeltaCompletionContext,
    receipt_id: &str,
) -> Result<(), PeerSyncError> {
    if delivery.source_device_id != context.source_device_id
        || delivery.lane != CompletionLane::Delta.as_str()
        || delivery.completion_lease_id != context.operation_id
        || delivery.manifest_id != context.manifest_id
        || delivery.receipt_id != receipt_id
    {
        return invalid("pending delta completion delivery does not match");
    }
    Ok(())
}

fn valid_generation(identity: &SyncGenerationIdentity) -> bool {
    !identity.generation_id.is_empty()
        && is_lower_hex_256(&identity.manifest_hash)
        && is_canonical_decimal(&identity.generation_sequence)
}

fn is_canonical_decimal(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn validate_uuid(value: &str, label: &str) -> Result<(), PeerSyncError> {
    if uuid::Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
    {
        Ok(())
    } else {
        invalid(&format!("{label} identifier is invalid"))
    }
}

fn validate_uuid_v4(value: &str, label: &str) -> Result<(), PeerSyncError> {
    let parsed = uuid::Uuid::parse_str(value)
        .map_err(|_| PeerSyncError::Validation(format!("{label} identifier is invalid")))?;
    if parsed.get_version_num() == 4
        && parsed.get_variant() == uuid::Variant::RFC4122
        && parsed.to_string() == value
    {
        Ok(())
    } else {
        invalid(&format!("{label} identifier is invalid"))
    }
}

fn create_owner_only(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(windows)]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = destination
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(not(windows))]
fn replace_file_atomic(source: &Path, destination: &Path) -> Result<(), PeerSyncError> {
    fs::rename(source, destination)?;
    let parent = destination.parent().ok_or_else(|| {
        PeerSyncError::Storage("delta completion journal has no parent".to_owned())
    })?;
    let _ = sync_directory(parent)?;
    Ok(())
}

fn invalid<T>(message: &str) -> Result<T, PeerSyncError> {
    Err(PeerSyncError::Validation(message.to_owned()))
}
