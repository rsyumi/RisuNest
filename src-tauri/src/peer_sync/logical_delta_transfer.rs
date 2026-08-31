use super::PeerSyncError;
use crate::{
    asset_repository::PayloadCas,
    local_backup::{CancellationProbe, NeverCancelled},
    peer_sync::logical_delta::decode_logical_record_key,
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Read},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalDeltaApplyOperation {
    Put {
        key: String,
        object_hash: String,
        dependencies: Vec<String>,
    },
    Delete {
        key: String,
        deleted_generation_sequence: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyLogicalDeltaPlan {
    pub expected_local_revision: i64,
    pub expected_base_manifest_hash: String,
    pub expected_remote_generation: String,
    pub apply: Vec<LogicalDeltaApplyOperation>,
    pub preserve_local_keys: Vec<String>,
    pub candidate_object_hashes: Vec<String>,
    pub next_base_manifest_hash: String,
    pub next_base_generation_sequence: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalDeltaObject {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalDeltaTransferSelection {
    reused_from_local_manifest: Vec<LogicalDeltaObject>,
    reused_from_cas: Vec<LogicalDeltaObject>,
    missing_objects: Vec<LogicalDeltaObject>,
}

impl LogicalDeltaTransferSelection {
    // Reuse accounting is asserted by the transfer-selection tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reused_from_local_manifest(&self) -> &[LogicalDeltaObject] {
        &self.reused_from_local_manifest
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn reused_from_cas(&self) -> &[LogicalDeltaObject] {
        &self.reused_from_cas
    }

    pub fn missing_objects(&self) -> &[LogicalDeltaObject] {
        &self.missing_objects
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalDeltaActivation {
    Activated {
        revision: i64,
    },
    AlreadyActive {
        revision: i64,
    },
    Conflict {
        actual_revision: i64,
        actual_base_manifest_hash: String,
    },
}

pub trait LogicalDeltaObjectSource {
    fn open_object(&mut self, object: &LogicalDeltaObject) -> Result<Box<dyn Read>, PeerSyncError>;
}

pub trait LogicalDeltaStagedTarget {
    type Stage;

    fn begin(&mut self, plan: &ReadyLogicalDeltaPlan) -> Result<Self::Stage, PeerSyncError>;

    fn can_activate_without_transfer(&self, _stage: &Self::Stage) -> bool {
        false
    }

    fn stage_payload(
        &mut self,
        stage: &mut Self::Stage,
        object: &LogicalDeltaObject,
        reader: &mut dyn Read,
    ) -> Result<(), PeerSyncError>;

    fn stage_database_changes(
        &mut self,
        stage: &mut Self::Stage,
        plan: &ReadyLogicalDeltaPlan,
    ) -> Result<(), PeerSyncError>;

    fn prepare_activation(&mut self, _stage: &mut Self::Stage) -> Result<(), PeerSyncError> {
        Ok(())
    }

    /// The expected revision and base checks, database activation, and common-base hash and
    /// generation-sequence update must be one atomic target transaction. An error must mean that
    /// transaction did not commit.
    fn activate_database_and_base_if_current(
        &mut self,
        stage: &mut Self::Stage,
        expected_local_revision: i64,
        expected_base_manifest_hash: &str,
        next_base_manifest_hash: &str,
        next_base_generation_sequence: &str,
    ) -> Result<LogicalDeltaActivation, PeerSyncError>;

    fn abort(&mut self, stage: Self::Stage) -> Result<(), PeerSyncError>;
}

pub fn select_missing_logical_delta_objects(
    plan: &ReadyLogicalDeltaPlan,
    local_manifest_object_hashes: &BTreeSet<String>,
    target_cas: &PayloadCas,
    remote_object_sizes: &BTreeMap<String, u64>,
) -> Result<LogicalDeltaTransferSelection, PeerSyncError> {
    validate_ready_plan(plan)?;
    let mut selection = LogicalDeltaTransferSelection {
        reused_from_local_manifest: Vec::new(),
        reused_from_cas: Vec::new(),
        missing_objects: Vec::new(),
    };
    for hash in &plan.candidate_object_hashes {
        let size = *remote_object_sizes.get(hash).ok_or_else(|| {
            PeerSyncError::Validation(format!(
                "logical delta remote manifest is missing object size for {hash}"
            ))
        })?;
        let object = LogicalDeltaObject {
            hash: hash.clone(),
            size,
        };
        if local_manifest_object_hashes.contains(hash) {
            selection.reused_from_local_manifest.push(object);
            continue;
        }
        match target_cas.stat_object(hash)? {
            Some(actual_size) if actual_size != size => {
                return Err(PeerSyncError::Validation(format!(
                    "logical delta object size mismatch for {hash}: expected {size}, found {actual_size}"
                )));
            }
            Some(_) => selection.reused_from_cas.push(object),
            None => selection.missing_objects.push(object),
        }
    }
    Ok(selection)
}

// Test-facing wrapper around the pre-activation pull entry point.
#[cfg_attr(not(test), allow(dead_code))]
pub fn execute_logical_delta_pull<S, T>(
    plan: &ReadyLogicalDeltaPlan,
    local_manifest_object_hashes: &BTreeSet<String>,
    target_cas: &PayloadCas,
    remote_object_sizes: &BTreeMap<String, u64>,
    source: &mut S,
    target: &mut T,
) -> Result<LogicalDeltaActivation, PeerSyncError>
where
    S: LogicalDeltaObjectSource,
    T: LogicalDeltaStagedTarget,
{
    execute_logical_delta_pull_with_pre_activation(
        plan,
        local_manifest_object_hashes,
        target_cas,
        remote_object_sizes,
        source,
        target,
        |_| Ok(()),
        &NeverCancelled,
    )
}

pub(crate) fn execute_logical_delta_pull_with_pre_activation<S, T, F>(
    plan: &ReadyLogicalDeltaPlan,
    local_manifest_object_hashes: &BTreeSet<String>,
    target_cas: &PayloadCas,
    remote_object_sizes: &BTreeMap<String, u64>,
    source: &mut S,
    target: &mut T,
    before_activation: F,
    cancellation: &dyn CancellationProbe,
) -> Result<LogicalDeltaActivation, PeerSyncError>
where
    S: LogicalDeltaObjectSource,
    T: LogicalDeltaStagedTarget,
    F: FnOnce(&LogicalDeltaTransferSelection) -> Result<(), PeerSyncError>,
{
    check_cancelled(cancellation)?;
    let mut stage = target.begin(plan)?;
    let result = (|| {
        if target.can_activate_without_transfer(&stage) {
            check_cancelled(cancellation)?;
            before_activation(&LogicalDeltaTransferSelection {
                reused_from_local_manifest: Vec::new(),
                reused_from_cas: Vec::new(),
                missing_objects: Vec::new(),
            })?;
            check_cancelled(cancellation)?;
            target.prepare_activation(&mut stage)?;
            check_cancelled(cancellation)?;
            return target.activate_database_and_base_if_current(
                &mut stage,
                plan.expected_local_revision,
                &plan.expected_base_manifest_hash,
                &plan.next_base_manifest_hash,
                &plan.next_base_generation_sequence,
            );
        }
        let selection = select_missing_logical_delta_objects(
            plan,
            local_manifest_object_hashes,
            target_cas,
            remote_object_sizes,
        )?;
        check_cancelled(cancellation)?;
        before_activation(&selection)?;
        check_cancelled(cancellation)?;
        for object in selection.missing_objects() {
            check_cancelled(cancellation)?;
            let mut source_reader = source.open_object(object)?;
            let mut cancellable_reader = CancellableReader {
                inner: source_reader.as_mut(),
                cancellation,
            };
            let mut verified_reader = VerifiedObjectReader::new(&mut cancellable_reader);
            let staged = target.stage_payload(&mut stage, object, &mut verified_reader);
            check_cancelled(cancellation)?;
            staged?;
            let verified = verified_reader.finish(object);
            check_cancelled(cancellation)?;
            verified?;
        }
        target.stage_database_changes(&mut stage, plan)?;
        target.prepare_activation(&mut stage)?;
        check_cancelled(cancellation)?;
        target.activate_database_and_base_if_current(
            &mut stage,
            plan.expected_local_revision,
            &plan.expected_base_manifest_hash,
            &plan.next_base_manifest_hash,
            &plan.next_base_generation_sequence,
        )
    })();

    match result {
        Ok(activation @ LogicalDeltaActivation::Activated { .. }) => Ok(activation),
        Ok(activation) => {
            target.abort(stage)?;
            Ok(activation)
        }
        Err(primary) => match target.abort(stage) {
            Ok(()) => Err(primary),
            Err(abort) => Err(PeerSyncError::Storage(format!(
                "{primary}; logical delta staging abort failed: {abort}"
            ))),
        },
    }
}

fn check_cancelled(cancellation: &dyn CancellationProbe) -> Result<(), PeerSyncError> {
    if cancellation.is_cancelled() {
        Err(PeerSyncError::Cancelled)
    } else {
        Ok(())
    }
}

struct CancellableReader<'a> {
    inner: &'a mut dyn Read,
    cancellation: &'a dyn CancellationProbe,
}

impl Read for CancellableReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.cancellation.is_cancelled() {
            return Err(io::Error::other("logical delta transfer cancelled"));
        }
        self.inner.read(output)
    }
}

fn validate_ready_plan(plan: &ReadyLogicalDeltaPlan) -> Result<(), PeerSyncError> {
    if plan.expected_local_revision < 0 {
        return validation("logical delta expected local revision must be nonnegative");
    }
    validate_hash(
        &plan.expected_base_manifest_hash,
        "logical delta expected base manifest hash",
    )?;
    validate_hash(
        &plan.next_base_manifest_hash,
        "logical delta next base manifest hash",
    )?;
    validate_generation_sequence(
        &plan.next_base_generation_sequence,
        "logical delta next base generation sequence",
    )?;
    if plan.expected_remote_generation.is_empty() {
        return validation("logical delta expected remote generation must be nonempty");
    }

    let mut required_objects = BTreeSet::new();
    let mut operation_keys = BTreeSet::new();
    let mut previous_operation_key: Option<&str> = None;
    for operation in &plan.apply {
        let key = match operation {
            LogicalDeltaApplyOperation::Put {
                key,
                object_hash,
                dependencies,
            } => {
                validate_hash(object_hash, "logical delta record object hash")?;
                required_objects.insert(object_hash.clone());
                let mut previous: Option<&str> = None;
                for dependency in dependencies {
                    validate_hash(dependency, "logical delta record dependency")?;
                    if previous.is_some_and(|previous| previous >= dependency.as_str()) {
                        return validation(
                            "logical delta record dependencies must be sorted and unique",
                        );
                    }
                    required_objects.insert(dependency.clone());
                    previous = Some(dependency);
                }
                key
            }
            LogicalDeltaApplyOperation::Delete {
                key,
                deleted_generation_sequence,
            } => {
                validate_generation_sequence(
                    deleted_generation_sequence,
                    "logical delta deleted generation sequence",
                )?;
                key
            }
        };
        decode_logical_record_key(key).map_err(|error| {
            PeerSyncError::Validation(format!("logical delta record key is invalid: {error}"))
        })?;
        if previous_operation_key.is_some_and(|previous| previous >= key.as_str()) {
            return validation("logical delta apply keys must be sorted and unique");
        }
        previous_operation_key = Some(key);
        operation_keys.insert(key.clone());
    }

    let mut previous_preserved_key: Option<&str> = None;
    for key in &plan.preserve_local_keys {
        decode_logical_record_key(key).map_err(|error| {
            PeerSyncError::Validation(format!(
                "logical delta preserved record key is invalid: {error}"
            ))
        })?;
        if previous_preserved_key.is_some_and(|previous| previous >= key.as_str()) {
            return validation("logical delta preserved keys must be sorted and unique");
        }
        if operation_keys.contains(key) {
            return validation("logical delta cannot both apply and preserve one record key");
        }
        previous_preserved_key = Some(key);
    }
    let candidates = plan
        .candidate_object_hashes
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if candidates.len() != plan.candidate_object_hashes.len()
        || candidates.iter().ne(plan.candidate_object_hashes.iter())
    {
        return validation("logical delta candidate object hashes must be sorted and unique");
    }
    for hash in &plan.candidate_object_hashes {
        validate_hash(hash, "logical delta candidate object hash")?;
    }
    if candidates != required_objects {
        return validation(
            "logical delta candidate object hashes do not match the ready plan record graph",
        );
    }
    Ok(())
}

fn validate_hash(hash: &str, description: &str) -> Result<(), PeerSyncError> {
    if hash.len() == 64
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Ok(());
    }
    validation(format!(
        "{description} must be 64 lowercase hexadecimal characters"
    ))
}

fn validate_generation_sequence(sequence: &str, description: &str) -> Result<(), PeerSyncError> {
    let canonical = sequence == "0"
        || (sequence.len() <= 64
            && sequence
                .bytes()
                .next()
                .is_some_and(|byte| (b'1'..=b'9').contains(&byte))
            && sequence.bytes().skip(1).all(|byte| byte.is_ascii_digit()));
    if canonical {
        return Ok(());
    }
    validation(format!("{description} must be canonical unsigned decimal"))
}

fn validation<T>(message: impl Into<String>) -> Result<T, PeerSyncError> {
    Err(PeerSyncError::Validation(message.into()))
}

struct VerifiedObjectReader<'a> {
    inner: &'a mut dyn Read,
    hasher: Sha256,
    bytes: u64,
}

impl<'a> VerifiedObjectReader<'a> {
    fn new(inner: &'a mut dyn Read) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(mut self, object: &LogicalDeltaObject) -> Result<(), PeerSyncError> {
        let mut extra = [0_u8; 1];
        if self.read(&mut extra)? != 0 || self.bytes != object.size {
            return Err(PeerSyncError::Validation(format!(
                "logical delta object {} did not transfer its exact declared size",
                object.hash
            )));
        }
        if hex::encode(self.hasher.finalize()) != object.hash {
            return Err(PeerSyncError::WholeObjectHashMismatch {
                object: object.hash.clone(),
            });
        }
        Ok(())
    }
}

impl Read for VerifiedObjectReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(output)?;
        self.hasher.update(&output[..read]);
        self.bytes = self
            .bytes
            .checked_add(read as u64)
            .ok_or_else(|| std::io::Error::other("logical delta transfer byte count overflow"))?;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        execute_logical_delta_pull, execute_logical_delta_pull_with_pre_activation,
        select_missing_logical_delta_objects, LogicalDeltaActivation, LogicalDeltaApplyOperation,
        LogicalDeltaObject, LogicalDeltaObjectSource, LogicalDeltaStagedTarget,
        ReadyLogicalDeltaPlan,
    };
    use crate::{
        asset_repository::PayloadCas,
        local_backup::{AtomicCancellation, CancellationProbe, NeverCancelled},
        peer_sync::PeerSyncError,
    };
    use sha2::{Digest, Sha256};
    use std::{
        collections::{BTreeMap, BTreeSet},
        io::{self, Cursor, Read},
        sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering},
            Arc,
        },
    };

    fn hash(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    fn ready_plan(
        apply: Vec<LogicalDeltaApplyOperation>,
        candidate_object_hashes: Vec<String>,
    ) -> ReadyLogicalDeltaPlan {
        ReadyLogicalDeltaPlan {
            expected_local_revision: 7,
            expected_base_manifest_hash: "1".repeat(64),
            expected_remote_generation: "remote-generation-8".to_owned(),
            apply,
            preserve_local_keys: Vec::new(),
            candidate_object_hashes,
            next_base_manifest_hash: "2".repeat(64),
            next_base_generation_sequence: "8".to_owned(),
        }
    }

    fn put(
        key: &str,
        object_hash: String,
        mut dependencies: Vec<String>,
    ) -> LogicalDeltaApplyOperation {
        dependencies.sort();
        LogicalDeltaApplyOperation::Put {
            key: key.to_owned(),
            object_hash,
            dependencies,
        }
    }

    #[test]
    fn exact_cas_stat_reuses_local_and_unreferenced_objects_and_selects_only_missing_candidates() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let local = cas.prepare_bytes(b"local-object").unwrap();
        let deduplicated = cas.prepare_bytes(b"unreferenced-but-present").unwrap();
        let missing_hash = hash(b"missing-object");
        let mut candidates = vec![
            local.content_hash.clone(),
            deduplicated.content_hash.clone(),
            missing_hash.clone(),
        ];
        candidates.sort();
        let plan = ready_plan(
            vec![put(
                "r1:root",
                local.content_hash.clone(),
                vec![deduplicated.content_hash.clone(), missing_hash.clone()],
            )],
            candidates.clone(),
        );
        let local_manifest_objects = BTreeSet::from([local.content_hash.clone()]);
        let remote_object_sizes = BTreeMap::from([
            (local.content_hash.clone(), local.byte_size),
            (deduplicated.content_hash.clone(), deduplicated.byte_size),
            (missing_hash.clone(), b"missing-object".len() as u64),
        ]);

        let selection = select_missing_logical_delta_objects(
            &plan,
            &local_manifest_objects,
            &cas,
            &remote_object_sizes,
        )
        .unwrap();

        assert_eq!(
            selection.reused_from_local_manifest(),
            &[LogicalDeltaObject {
                hash: local.content_hash,
                size: local.byte_size,
            }]
        );
        assert_eq!(
            selection.reused_from_cas(),
            &[LogicalDeltaObject {
                hash: deduplicated.content_hash,
                size: deduplicated.byte_size,
            }]
        );
        assert_eq!(
            selection.missing_objects(),
            &[LogicalDeltaObject {
                hash: missing_hash,
                size: b"missing-object".len() as u64,
            }]
        );
    }

    #[test]
    fn selector_rejects_remote_size_mismatch_for_an_existing_immutable_object() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let prepared = cas.prepare_bytes(b"immutable").unwrap();
        let plan = ready_plan(
            vec![put("r1:root", prepared.content_hash.clone(), vec![])],
            vec![prepared.content_hash.clone()],
        );
        let error = select_missing_logical_delta_objects(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::from([(prepared.content_hash, prepared.byte_size + 1)]),
        )
        .unwrap_err();

        assert!(matches!(error, PeerSyncError::Validation(message) if message.contains("size")));
    }

    struct FixtureSource {
        objects: BTreeMap<String, Vec<u8>>,
        content_gets: usize,
    }

    impl LogicalDeltaObjectSource for FixtureSource {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            self.content_gets += 1;
            let bytes = self
                .objects
                .get(&object.hash)
                .ok_or_else(|| PeerSyncError::Transport("missing fixture object".to_owned()))?;
            Ok(Box::new(Cursor::new(bytes.clone())))
        }
    }

    #[derive(Default)]
    struct FixtureStage {
        payloads: Vec<String>,
        database_staged: bool,
    }

    struct FixtureTarget {
        active_revision: i64,
        active_base: String,
        active_base_generation_sequence: String,
        locally_resolvable: BTreeSet<String>,
        events: Vec<String>,
        aborts: usize,
        fail_database_stage: bool,
        cancel_on_prepare: Option<Arc<AtomicBool>>,
        activate_without_transfer: bool,
    }

    impl FixtureTarget {
        fn new() -> Self {
            Self {
                active_revision: 7,
                active_base: "1".repeat(64),
                active_base_generation_sequence: "7".to_owned(),
                locally_resolvable: BTreeSet::new(),
                events: Vec::new(),
                aborts: 0,
                fail_database_stage: false,
                cancel_on_prepare: None,
                activate_without_transfer: false,
            }
        }
    }

    impl LogicalDeltaStagedTarget for FixtureTarget {
        type Stage = FixtureStage;

        fn begin(&mut self, _plan: &ReadyLogicalDeltaPlan) -> Result<Self::Stage, PeerSyncError> {
            self.events.push("begin".to_owned());
            Ok(FixtureStage::default())
        }

        fn can_activate_without_transfer(&self, _stage: &Self::Stage) -> bool {
            self.activate_without_transfer
        }

        fn stage_payload(
            &mut self,
            stage: &mut Self::Stage,
            object: &LogicalDeltaObject,
            reader: &mut dyn Read,
        ) -> Result<(), PeerSyncError> {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes)?;
            self.events.push(format!("payload:{}", object.hash));
            stage.payloads.push(object.hash.clone());
            Ok(())
        }

        fn stage_database_changes(
            &mut self,
            stage: &mut Self::Stage,
            plan: &ReadyLogicalDeltaPlan,
        ) -> Result<(), PeerSyncError> {
            self.events.push("database".to_owned());
            if self.fail_database_stage {
                return Err(PeerSyncError::Storage("database staging failed".to_owned()));
            }
            if plan.candidate_object_hashes.iter().any(|hash| {
                !stage.payloads.contains(hash) && !self.locally_resolvable.contains(hash)
            }) {
                return Err(PeerSyncError::Storage(
                    "database staging could not resolve a logical object".to_owned(),
                ));
            }
            stage.database_staged = true;
            Ok(())
        }

        fn prepare_activation(&mut self, _stage: &mut Self::Stage) -> Result<(), PeerSyncError> {
            self.events.push("prepare".to_owned());
            if let Some(cancelled) = &self.cancel_on_prepare {
                cancelled.store(true, Ordering::SeqCst);
            }
            Ok(())
        }

        fn activate_database_and_base_if_current(
            &mut self,
            stage: &mut Self::Stage,
            expected_local_revision: i64,
            expected_base_manifest_hash: &str,
            next_base_manifest_hash: &str,
            next_base_generation_sequence: &str,
        ) -> Result<LogicalDeltaActivation, PeerSyncError> {
            self.events.push("activate".to_owned());
            if self.active_revision != expected_local_revision
                || self.active_base != expected_base_manifest_hash
            {
                return Ok(LogicalDeltaActivation::Conflict {
                    actual_revision: self.active_revision,
                    actual_base_manifest_hash: self.active_base.clone(),
                });
            }
            assert!(stage.database_staged);
            self.active_revision += 1;
            self.active_base = next_base_manifest_hash.to_owned();
            self.active_base_generation_sequence = next_base_generation_sequence.to_owned();
            Ok(LogicalDeltaActivation::Activated {
                revision: self.active_revision,
            })
        }

        fn abort(&mut self, _stage: Self::Stage) -> Result<(), PeerSyncError> {
            self.events.push("abort".to_owned());
            self.aborts += 1;
            Ok(())
        }
    }

    #[test]
    fn no_op_ready_plan_performs_zero_content_gets() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let plan = ready_plan(vec![], vec![]);
        let mut source = FixtureSource {
            objects: BTreeMap::new(),
            content_gets: 0,
        };
        let mut target = FixtureTarget::new();

        let activation = execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::new(),
            &mut source,
            &mut target,
        )
        .unwrap();

        assert_eq!(source.content_gets, 0);
        assert_eq!(
            activation,
            LogicalDeltaActivation::Activated { revision: 8 }
        );
        assert_eq!(target.events, ["begin", "database", "prepare", "activate"]);
    }

    #[test]
    fn next_common_base_generation_sequence_must_be_canonical() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let mut plan = ready_plan(vec![], vec![]);
        plan.next_base_generation_sequence = "08".to_owned();

        let error =
            select_missing_logical_delta_objects(&plan, &BTreeSet::new(), &cas, &BTreeMap::new())
                .unwrap_err();

        assert!(
            matches!(error, PeerSyncError::Validation(message) if message.contains("generation sequence"))
        );
    }

    #[test]
    fn ready_plan_rejects_invalid_or_duplicate_record_operations() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let invalid_tombstone = ready_plan(
            vec![LogicalDeltaApplyOperation::Delete {
                key: "r1:root".to_owned(),
                deleted_generation_sequence: "08".to_owned(),
            }],
            vec![],
        );
        let error = select_missing_logical_delta_objects(
            &invalid_tombstone,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(
            matches!(error, PeerSyncError::Validation(message) if message.contains("deleted generation sequence"))
        );

        let duplicate_key = ready_plan(
            vec![
                LogicalDeltaApplyOperation::Delete {
                    key: "r1:root".to_owned(),
                    deleted_generation_sequence: "8".to_owned(),
                },
                LogicalDeltaApplyOperation::Delete {
                    key: "r1:root".to_owned(),
                    deleted_generation_sequence: "8".to_owned(),
                },
            ],
            vec![],
        );
        let error = select_missing_logical_delta_objects(
            &duplicate_key,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(
            matches!(error, PeerSyncError::Validation(message) if message.contains("sorted and unique"))
        );
    }

    #[test]
    fn local_manifest_object_without_cas_file_is_reconstructed_without_content_get() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let local_record = b"pinned-pds-record".to_vec();
        let local_record_hash = hash(&local_record);
        let plan = ready_plan(
            vec![put("r1:root", local_record_hash.clone(), vec![])],
            vec![local_record_hash.clone()],
        );
        let mut source = FixtureSource {
            objects: BTreeMap::from([(local_record_hash.clone(), local_record)]),
            content_gets: 0,
        };
        let mut target = FixtureTarget::new();
        target.locally_resolvable.insert(local_record_hash.clone());

        let activation = execute_logical_delta_pull(
            &plan,
            &BTreeSet::from([local_record_hash.clone()]),
            &cas,
            &BTreeMap::from([(local_record_hash.clone(), b"pinned-pds-record".len() as u64)]),
            &mut source,
            &mut target,
        )
        .unwrap();

        assert_eq!(
            activation,
            LogicalDeltaActivation::Activated { revision: 8 }
        );
        assert_eq!(source.content_gets, 0);
        assert_eq!(target.events, ["begin", "database", "prepare", "activate"]);
    }

    #[test]
    fn selection_failure_after_begin_aborts_the_new_stage() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let payload_hash = hash(b"remote-payload");
        let plan = ready_plan(
            vec![put("r1:asset:WyJhIl0", payload_hash.clone(), vec![])],
            vec![payload_hash],
        );
        let mut source = FixtureSource {
            objects: BTreeMap::new(),
            content_gets: 0,
        };
        let mut target = FixtureTarget::new();

        assert!(matches!(
            execute_logical_delta_pull(
                &plan,
                &BTreeSet::new(),
                &cas,
                &BTreeMap::new(),
                &mut source,
                &mut target,
            ),
            Err(PeerSyncError::Validation(_))
        ));
        assert_eq!(source.content_gets, 0);
        assert_eq!(target.aborts, 1);
        assert_eq!(target.events, ["begin", "abort"]);
    }

    #[test]
    fn verified_payloads_stage_before_database_and_atomic_base_activation() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let payload = b"remote-payload".to_vec();
        let payload_hash = hash(&payload);
        let plan = ready_plan(
            vec![put("r1:asset:WyJhIl0", payload_hash.clone(), vec![])],
            vec![payload_hash.clone()],
        );
        let mut source = FixtureSource {
            objects: BTreeMap::from([(payload_hash.clone(), payload)]),
            content_gets: 0,
        };
        let mut target = FixtureTarget::new();

        let activation = execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::from([(payload_hash.clone(), b"remote-payload".len() as u64)]),
            &mut source,
            &mut target,
        )
        .unwrap();

        assert_eq!(
            activation,
            LogicalDeltaActivation::Activated { revision: 8 }
        );
        assert_eq!(source.content_gets, 1);
        assert_eq!(
            target.events,
            [
                "begin".to_owned(),
                format!("payload:{payload_hash}"),
                "database".to_owned(),
                "prepare".to_owned(),
                "activate".to_owned(),
            ]
        );
        assert_eq!(target.active_revision, 8);
        assert_eq!(target.active_base, "2".repeat(64));
        assert_eq!(target.active_base_generation_sequence, "8");
        assert_eq!(target.aborts, 0);
    }

    #[test]
    fn pre_activation_evidence_is_persisted_before_payload_and_database_staging() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let payload = b"remote-payload".to_vec();
        let payload_hash = hash(&payload);
        let plan = ready_plan(
            vec![put("r1:asset:WyJhIl0", payload_hash.clone(), vec![])],
            vec![payload_hash.clone()],
        );
        let mut source = FixtureSource {
            objects: BTreeMap::from([(payload_hash.clone(), payload)]),
            content_gets: 0,
        };
        let mut target = FixtureTarget::new();
        let mut evidence = None;

        let error = execute_logical_delta_pull_with_pre_activation(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::from([(payload_hash.clone(), b"remote-payload".len() as u64)]),
            &mut source,
            &mut target,
            |selection| {
                evidence = Some(selection.missing_objects().to_vec());
                Err(PeerSyncError::Storage(
                    "simulated durable evidence failure".to_owned(),
                ))
            },
            &NeverCancelled,
        )
        .unwrap_err();

        assert!(matches!(error, PeerSyncError::Storage(message) if message.contains("evidence")));
        assert_eq!(
            evidence,
            Some(vec![LogicalDeltaObject {
                hash: payload_hash,
                size: b"remote-payload".len() as u64,
            }])
        );
        assert_eq!(source.content_gets, 0);
        assert_eq!(target.active_revision, 7);
        assert_eq!(target.active_base, "1".repeat(64));
        assert_eq!(target.aborts, 1);
        assert!(!target.events.iter().any(|event| event == "database"));
        assert!(!target.events.iter().any(|event| event == "prepare"));
        assert_eq!(target.events.last().map(String::as_str), Some("abort"));
        assert!(!target.events.iter().any(|event| event == "activate"));
    }

    struct CancellingSource {
        object: Vec<u8>,
        cancelled: Arc<AtomicBool>,
        content_gets: usize,
    }

    impl LogicalDeltaObjectSource for CancellingSource {
        fn open_object(
            &mut self,
            _object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            self.content_gets += 1;
            Ok(Box::new(CancelAfterFirstRead {
                inner: Cursor::new(self.object.clone()),
                cancelled: Arc::clone(&self.cancelled),
                first_read: true,
            }))
        }
    }

    struct CancelAfterFirstRead {
        inner: Cursor<Vec<u8>>,
        cancelled: Arc<AtomicBool>,
        first_read: bool,
    }

    struct CancelBetweenObjects {
        first_object_eof: Arc<AtomicBool>,
        checks_after_eof: AtomicUsize,
    }

    impl CancellationProbe for CancelBetweenObjects {
        fn is_cancelled(&self) -> bool {
            if !self.first_object_eof.load(Ordering::SeqCst) {
                return false;
            }
            self.checks_after_eof.fetch_add(1, Ordering::SeqCst) >= 2
        }
    }

    struct EofSignallingSource {
        objects: BTreeMap<String, Vec<u8>>,
        first_object_eof: Arc<AtomicBool>,
        content_gets: usize,
    }

    impl LogicalDeltaObjectSource for EofSignallingSource {
        fn open_object(
            &mut self,
            object: &LogicalDeltaObject,
        ) -> Result<Box<dyn Read>, PeerSyncError> {
            self.content_gets += 1;
            let bytes = self
                .objects
                .get(&object.hash)
                .cloned()
                .ok_or_else(|| PeerSyncError::Storage("missing fixture object".to_owned()))?;
            Ok(Box::new(EofSignallingReader {
                inner: Cursor::new(bytes),
                eof: Arc::clone(&self.first_object_eof),
            }))
        }
    }

    struct EofSignallingReader {
        inner: Cursor<Vec<u8>>,
        eof: Arc<AtomicBool>,
    }

    impl Read for EofSignallingReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(output)?;
            if read == 0 {
                self.eof.store(true, Ordering::SeqCst);
            }
            Ok(read)
        }
    }

    impl Read for CancelAfterFirstRead {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let limit = output.len().min(4);
            let read = self.inner.read(&mut output[..limit])?;
            if self.first_read && read != 0 {
                self.first_read = false;
                self.cancelled.store(true, Ordering::SeqCst);
            }
            Ok(read)
        }
    }

    #[test]
    fn entry_cancellation_starts_no_staging_or_common_base_work() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let plan = ready_plan(vec![], vec![]);
        let mut source = FixtureSource {
            objects: BTreeMap::new(),
            content_gets: 0,
        };
        let cancellation = AtomicCancellation::new(Arc::new(AtomicBool::new(true)));
        let mut target = FixtureTarget::new();

        let error = execute_logical_delta_pull_with_pre_activation(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::new(),
            &mut source,
            &mut target,
            |_| Ok(()),
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error, PeerSyncError::Cancelled);
        assert!(target.events.is_empty());
        assert_eq!(target.aborts, 0);
    }

    #[test]
    fn cancellation_after_prepare_aborts_before_activation() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let payload = b"remote-payload".to_vec();
        let payload_hash = hash(&payload);
        let plan = ready_plan(
            vec![put("r1:asset:WyJhIl0", payload_hash.clone(), vec![])],
            vec![payload_hash.clone()],
        );
        let mut source = FixtureSource {
            objects: BTreeMap::from([(payload_hash.clone(), payload)]),
            content_gets: 0,
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = AtomicCancellation::new(Arc::clone(&cancelled));
        let mut target = FixtureTarget::new();
        target.cancel_on_prepare = Some(cancelled);

        let error = execute_logical_delta_pull_with_pre_activation(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::from([(payload_hash, b"remote-payload".len() as u64)]),
            &mut source,
            &mut target,
            |_| Ok(()),
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error, PeerSyncError::Cancelled);
        assert_eq!(target.active_revision, 7);
        assert_eq!(target.active_base, "1".repeat(64));
        assert_eq!(target.aborts, 1);
        assert_eq!(target.events.last().map(String::as_str), Some("abort"));
        assert!(!target.events.iter().any(|event| event == "activate"));
    }

    #[test]
    fn cancellation_during_stage_payload_aborts_without_retrying_the_reader() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let payload = b"remote-payload".to_vec();
        let payload_hash = hash(&payload);
        let plan = ready_plan(
            vec![put("r1:asset:WyJhIl0", payload_hash.clone(), vec![])],
            vec![payload_hash.clone()],
        );
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = AtomicCancellation::new(Arc::clone(&cancelled));
        let mut source = CancellingSource {
            object: payload,
            cancelled,
            content_gets: 0,
        };
        let mut target = FixtureTarget::new();

        let error = execute_logical_delta_pull_with_pre_activation(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::from([(payload_hash, b"remote-payload".len() as u64)]),
            &mut source,
            &mut target,
            |_| Ok(()),
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error, PeerSyncError::Cancelled);
        assert_eq!(source.content_gets, 1);
        assert_eq!(target.active_revision, 7);
        assert_eq!(target.aborts, 1);
        assert!(!target.events.iter().any(|event| event == "database"));
        assert!(!target.events.iter().any(|event| event == "activate"));
    }

    #[test]
    fn cancellation_in_pre_activation_callback_starts_no_object_read() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let payload = b"remote-payload".to_vec();
        let payload_hash = hash(&payload);
        let plan = ready_plan(
            vec![put("r1:asset:WyJhIl0", payload_hash.clone(), vec![])],
            vec![payload_hash.clone()],
        );
        let mut source = FixtureSource {
            objects: BTreeMap::from([(payload_hash.clone(), payload)]),
            content_gets: 0,
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = AtomicCancellation::new(Arc::clone(&cancelled));
        let mut target = FixtureTarget::new();

        let error = execute_logical_delta_pull_with_pre_activation(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::from([(payload_hash, b"remote-payload".len() as u64)]),
            &mut source,
            &mut target,
            |_| {
                cancelled.store(true, Ordering::SeqCst);
                Ok(())
            },
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error, PeerSyncError::Cancelled);
        assert_eq!(source.content_gets, 0);
        assert_eq!(target.active_revision, 7);
        assert_eq!(target.aborts, 1);
        assert!(!target
            .events
            .iter()
            .any(|event| event.starts_with("payload:")));
        assert!(!target.events.iter().any(|event| event == "database"));
    }

    #[test]
    fn cancellation_after_empty_transfer_selection_stages_no_database_work() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let payload = b"locally-resolvable-payload".to_vec();
        let payload_hash = hash(&payload);
        let plan = ready_plan(
            vec![put("r1:asset:WyJhIl0", payload_hash.clone(), vec![])],
            vec![payload_hash.clone()],
        );
        let mut source = FixtureSource {
            objects: BTreeMap::new(),
            content_gets: 0,
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = AtomicCancellation::new(Arc::clone(&cancelled));
        let mut target = FixtureTarget::new();
        target.locally_resolvable.insert(payload_hash.clone());

        let error = execute_logical_delta_pull_with_pre_activation(
            &plan,
            &BTreeSet::from([payload_hash.clone()]),
            &cas,
            &BTreeMap::from([(payload_hash, payload.len() as u64)]),
            &mut source,
            &mut target,
            |_| {
                cancelled.store(true, Ordering::SeqCst);
                Ok(())
            },
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error, PeerSyncError::Cancelled);
        assert_eq!(source.content_gets, 0);
        assert_eq!(target.active_revision, 7);
        assert_eq!(target.aborts, 1);
        assert_eq!(target.events, ["begin", "abort"]);
    }

    #[test]
    fn cancellation_after_no_op_callback_prepares_no_activation_work() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let plan = ready_plan(vec![], vec![]);
        let mut source = FixtureSource {
            objects: BTreeMap::new(),
            content_gets: 0,
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancellation = AtomicCancellation::new(Arc::clone(&cancelled));
        let mut target = FixtureTarget::new();
        target.activate_without_transfer = true;

        let error = execute_logical_delta_pull_with_pre_activation(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::new(),
            &mut source,
            &mut target,
            |_| {
                cancelled.store(true, Ordering::SeqCst);
                Ok(())
            },
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error, PeerSyncError::Cancelled);
        assert_eq!(source.content_gets, 0);
        assert_eq!(target.active_revision, 7);
        assert_eq!(target.aborts, 1);
        assert_eq!(target.events, ["begin", "abort"]);
    }

    #[test]
    fn cancellation_between_objects_starts_no_second_object_read() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let first_payload = b"first-remote-payload".to_vec();
        let second_payload = b"second-remote-payload".to_vec();
        let first_hash = hash(&first_payload);
        let second_hash = hash(&second_payload);
        let plan = ready_plan(
            vec![
                put("r1:asset:WyJhIl0", first_hash.clone(), vec![]),
                put("r1:asset:WyJiIl0", second_hash.clone(), vec![]),
            ],
            vec![first_hash.clone(), second_hash.clone()],
        );
        let first_object_eof = Arc::new(AtomicBool::new(false));
        let cancellation = CancelBetweenObjects {
            first_object_eof: Arc::clone(&first_object_eof),
            checks_after_eof: AtomicUsize::new(0),
        };
        let mut source = EofSignallingSource {
            objects: BTreeMap::from([
                (first_hash.clone(), first_payload),
                (second_hash.clone(), second_payload),
            ]),
            first_object_eof,
            content_gets: 0,
        };
        let mut target = FixtureTarget::new();

        let error = execute_logical_delta_pull_with_pre_activation(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::from([
                (first_hash, b"first-remote-payload".len() as u64),
                (second_hash, b"second-remote-payload".len() as u64),
            ]),
            &mut source,
            &mut target,
            |_| Ok(()),
            &cancellation,
        )
        .unwrap_err();

        assert_eq!(error, PeerSyncError::Cancelled);
        assert_eq!(source.content_gets, 1);
        assert_eq!(target.active_revision, 7);
        assert_eq!(target.aborts, 1);
        assert!(!target.events.iter().any(|event| event == "database"));
        assert!(!target.events.iter().any(|event| event == "activate"));
    }

    #[test]
    fn corrupt_payload_aborts_before_database_activation() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let expected_payload = b"expected-payload";
        let payload_hash = hash(expected_payload);
        let plan = ready_plan(
            vec![put("r1:asset:WyJhIl0", payload_hash.clone(), vec![])],
            vec![payload_hash.clone()],
        );
        let mut source = FixtureSource {
            objects: BTreeMap::from([(payload_hash.clone(), b"corrupt-payload!".to_vec())]),
            content_gets: 0,
        };
        let mut target = FixtureTarget::new();

        let error = execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::from([(payload_hash, expected_payload.len() as u64)]),
            &mut source,
            &mut target,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            PeerSyncError::WholeObjectHashMismatch { .. }
        ));
        assert_eq!(target.active_revision, 7);
        assert_eq!(target.active_base, "1".repeat(64));
        assert_eq!(target.active_base_generation_sequence, "7");
        assert_eq!(target.aborts, 1);
        assert!(!target.events.iter().any(|event| event == "database"));
        assert_eq!(target.events.last().map(String::as_str), Some("abort"));
    }

    #[test]
    fn staging_failure_aborts_and_preserves_complete_old_state() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let payload = b"remote-payload".to_vec();
        let payload_hash = hash(&payload);
        let plan = ready_plan(
            vec![put("r1:asset:WyJhIl0", payload_hash.clone(), vec![])],
            vec![payload_hash.clone()],
        );
        let mut source = FixtureSource {
            objects: BTreeMap::from([(payload_hash.clone(), payload)]),
            content_gets: 0,
        };
        let mut target = FixtureTarget::new();
        target.fail_database_stage = true;

        let error = execute_logical_delta_pull(
            &plan,
            &BTreeSet::new(),
            &cas,
            &BTreeMap::from([(payload_hash, b"remote-payload".len() as u64)]),
            &mut source,
            &mut target,
        )
        .unwrap_err();

        assert!(matches!(error, PeerSyncError::Storage(message) if message.contains("staging")));
        assert_eq!(target.active_revision, 7);
        assert_eq!(target.active_base, "1".repeat(64));
        assert_eq!(target.aborts, 1);
        assert_eq!(target.events.last().map(String::as_str), Some("abort"));
    }

    #[test]
    fn changed_revision_or_base_aborts_without_exposing_staged_state() {
        let directory = tempfile::tempdir().unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let plan = ready_plan(vec![], vec![]);
        for (actual_revision, actual_base) in [(8, "1".repeat(64)), (7, "9".repeat(64))] {
            let mut source = FixtureSource {
                objects: BTreeMap::new(),
                content_gets: 0,
            };
            let mut target = FixtureTarget::new();
            target.active_revision = actual_revision;
            target.active_base = actual_base.clone();

            let activation = execute_logical_delta_pull(
                &plan,
                &BTreeSet::new(),
                &cas,
                &BTreeMap::new(),
                &mut source,
                &mut target,
            )
            .unwrap();

            assert_eq!(
                activation,
                LogicalDeltaActivation::Conflict {
                    actual_revision,
                    actual_base_manifest_hash: actual_base.clone(),
                }
            );
            assert_eq!(target.active_revision, actual_revision);
            assert_eq!(target.active_base, actual_base);
            assert_eq!(target.active_base_generation_sequence, "7");
            assert_eq!(target.aborts, 1);
            assert_eq!(target.events.last().map(String::as_str), Some("abort"));
        }
    }
}
