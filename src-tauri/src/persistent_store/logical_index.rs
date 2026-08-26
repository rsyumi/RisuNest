use super::{
    active_generation, current_revision, logical_schema, AssetAlias, PersistentStore,
    PluginStorageMutation, ReadTarget, StoreError, StoreResult, WorkingSetCommit,
};
use crate::{
    asset_repository::{
        owner_manifest_codec::{decode_owner_manifest, encode_owner_manifest},
        PayloadCas,
    },
    peer_sync::logical_delta::{
        build_indexed_logical_manifest, decode_logical_record_key, encode_asset_alias_metadata,
        encode_logical_record, encode_logical_record_key, encode_message_page,
        BuiltIndexedLogicalManifest, EncodedLogicalObject, IndexedLogicalManifestBuilderInput,
        IndexedLogicalRecord, LogicalAssetAliasMetadata, LogicalManifestObject, LogicalOwnerHead,
        LogicalOwnerLocator, LogicalRecordEnvelope, LogicalRecordLocator,
        LOGICAL_MESSAGE_PAGE_SIZE,
    },
};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

const RECORD_KIND_ROOT: &str = "root";
const RECORD_KIND_PRESET: &str = "preset";
const RECORD_KIND_PLUGIN: &str = "plugin";
const RECORD_KIND_CHARACTER: &str = "character";
const RECORD_KIND_CONVERSATION: &str = "conversation";
const RECORD_KIND_ASSET: &str = "asset";
const RECORD_KIND_INLAY: &str = "inlay";
const RECORD_KIND_COLD: &str = "cold";

#[cfg(test)]
thread_local! {
    static MESSAGE_PAGE_REHASH_READS: std::cell::RefCell<Vec<(String, String, u64)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
fn reset_message_page_rehash_reads() {
    MESSAGE_PAGE_REHASH_READS.with(|reads| reads.borrow_mut().clear());
}

#[cfg(test)]
fn take_message_page_rehash_reads() -> Vec<(String, String, u64)> {
    MESSAGE_PAGE_REHASH_READS.with(|reads| std::mem::take(&mut *reads.borrow_mut()))
}

#[cfg(test)]
fn record_message_page_rehash_read(character_id: &str, conversation_id: &str, page_index: u64) {
    MESSAGE_PAGE_REHASH_READS.with(|reads| {
        reads.borrow_mut().push((
            character_id.to_owned(),
            conversation_id.to_owned(),
            page_index,
        ));
    });
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LogicalIndexBuildRequest {
    pub(crate) library_id: String,
    pub(crate) generation_id: String,
    pub(crate) generation_sequence: String,
    pub(crate) parent_generation_id: Option<String>,
    pub(crate) lease: Option<String>,
}

#[derive(Clone, Debug)]
struct PageSource {
    page_index: u64,
    first_message_index: u64,
    message_count: u64,
    object: LogicalManifestObject,
}

#[derive(Clone, Debug)]
struct ValidatedOwnerHead {
    head: LogicalOwnerHead,
    tuples: Option<Vec<Value>>,
    dependencies: Vec<LogicalManifestObject>,
}

#[derive(Debug)]
struct LogicalGenerationMetadata {
    library_id: String,
    generation_id: String,
    generation_sequence: String,
    parent_generation_id: Option<String>,
    source_revision: i64,
    state: String,
    manifest_hash: Option<String>,
}

#[derive(Clone, Copy)]
enum LogicalDeleteScope<'a> {
    RecordKey(&'a str),
    RecordKind(&'a str),
    Generation,
}

#[derive(Clone, Debug)]
pub(super) struct IncrementalLogicalCommit {
    library_id: String,
    generation_id: String,
    generation_sequence: String,
    pds_generation: String,
}

#[derive(Clone, Debug)]
pub(super) enum ConversationChange {
    Delete {
        character_id: String,
        conversation_id: String,
    },
    ReplaceRange {
        character_id: String,
        conversation_id: String,
        old_count: u64,
        new_count: u64,
        fixed_page_ranges: Vec<(u64, u64)>,
        tail_from_page: Option<u64>,
        was_new: bool,
    },
}

pub(super) fn push_conversation_change(
    changes: &mut Vec<ConversationChange>,
    next: ConversationChange,
) -> StoreResult<()> {
    let (next_character_id, next_conversation_id) = conversation_change_identity(&next);
    let Some(index) = changes.iter().position(|existing| {
        let (character_id, conversation_id) = conversation_change_identity(existing);
        character_id == next_character_id && conversation_id == next_conversation_id
    }) else {
        changes.push(next);
        return Ok(());
    };

    match (&mut changes[index], next) {
        (ConversationChange::Delete { .. }, ConversationChange::Delete { .. }) => {}
        (existing @ ConversationChange::Delete { .. }, mut replacement) => {
            if let ConversationChange::ReplaceRange { was_new, .. } = &mut replacement {
                *was_new = true;
            }
            *existing = replacement;
        }
        (
            existing @ ConversationChange::ReplaceRange { .. },
            deletion @ ConversationChange::Delete { .. },
        ) => {
            *existing = deletion;
        }
        (
            ConversationChange::ReplaceRange {
                old_count: _,
                new_count,
                fixed_page_ranges,
                tail_from_page,
                was_new: _,
                ..
            },
            ConversationChange::ReplaceRange {
                old_count: next_old_count,
                new_count: next_new_count,
                fixed_page_ranges: next_fixed_page_ranges,
                tail_from_page: next_tail_from_page,
                ..
            },
        ) => {
            if *new_count != next_old_count {
                return validation("conversation mutation counts are not sequential");
            }
            *new_count = next_new_count;
            *tail_from_page = match (*tail_from_page, next_tail_from_page) {
                (Some(current), Some(next)) => Some(current.min(next)),
                (Some(current), None) => Some(current),
                (None, Some(next)) => Some(next),
                (None, None) => None,
            };
            fixed_page_ranges.extend(next_fixed_page_ranges);
            merge_fixed_page_ranges(fixed_page_ranges, *tail_from_page)?;
        }
    }
    Ok(())
}

fn merge_fixed_page_ranges(
    ranges: &mut Vec<(u64, u64)>,
    tail_from_page: Option<u64>,
) -> StoreResult<()> {
    ranges.sort_unstable();
    let mut merged = Vec::with_capacity(ranges.len());
    for &(start, mut end) in ranges.iter() {
        if start >= end {
            return validation("conversation fixed page range is empty or reversed");
        }
        if let Some(tail) = tail_from_page {
            if start >= tail {
                continue;
            }
            end = end.min(tail);
        }
        if let Some((_, previous_end)) = merged.last_mut() {
            if start <= *previous_end {
                *previous_end = (*previous_end).max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    *ranges = merged;
    Ok(())
}

fn conversation_change_identity(change: &ConversationChange) -> (&str, &str) {
    match change {
        ConversationChange::Delete {
            character_id,
            conversation_id,
        }
        | ConversationChange::ReplaceRange {
            character_id,
            conversation_id,
            ..
        } => (character_id, conversation_id),
    }
}

pub(super) fn logical_index_is_active(connection: &Connection) -> StoreResult<bool> {
    let schema_exists: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = 'logical_library_head'
         )",
        [],
        |row| row.get(0),
    )?;
    if !schema_exists {
        return Ok(false);
    }
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM logical_library_head WHERE singleton = 1)",
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

pub(super) fn cleanup_abandoned_logical_staging(connection: &mut Connection) -> StoreResult<()> {
    let schema_exists: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM sqlite_master
            WHERE type = 'table' AND name = 'logical_sync_generations'
         )",
        [],
        |row| row.get(0),
    )?;
    if !schema_exists {
        return Ok(());
    }
    logical_schema::create_logical_schema(connection).map_err(schema_error)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let active = super::active_generation(&transaction)?;
    let abandoned = {
        let mut statement = transaction.prepare(
            "SELECT generation.library_id, generation.generation_id, generation.pds_generation
             FROM logical_sync_generations AS generation
             WHERE generation.pds_generation LIKE 'staging-logical-%'
               AND generation.pds_generation != ?1
               AND NOT EXISTS (
                   SELECT 1 FROM logical_library_head AS head
                   WHERE head.library_id = generation.library_id
                     AND head.generation_id = generation.generation_id
               )
               AND NOT EXISTS (
                   SELECT 1 FROM logical_generation_session_pins AS pin
                   WHERE pin.library_id = generation.library_id
                     AND pin.generation_id = generation.generation_id
               )",
        )?;
        let rows = statement
            .query_map([active], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows
    };
    for (library_id, generation_id, pds_generation) in abandoned {
        transaction.execute(
            "DELETE FROM logical_peer_common_bases
             WHERE library_id = ?1 AND generation_id = ?2",
            params![library_id, generation_id],
        )?;
        delete_logical_rows(
            &transaction,
            &library_id,
            &generation_id,
            LogicalDeleteScope::Generation,
        )?;
        transaction.execute(
            "DELETE FROM snapshot_leases WHERE generation = ?1",
            [pds_generation],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

impl PersistentStore {
    pub(crate) fn rebuild_logical_index(
        &mut self,
        cas: &PayloadCas,
        request: LogicalIndexBuildRequest,
    ) -> StoreResult<BuiltIndexedLogicalManifest> {
        self.initialize_logical_index_building(cas, request)?;
        self.seal_active_logical_generation(cas)
    }

    pub(crate) fn initialize_logical_index_building(
        &mut self,
        cas: &PayloadCas,
        request: LogicalIndexBuildRequest,
    ) -> StoreResult<()> {
        if request.lease.is_some() {
            return validation("logical index build from a detached revision is not supported");
        }
        logical_schema::create_logical_schema(&self.connection).map_err(schema_error)?;
        logical_schema::validate_logical_schema(&self.connection).map_err(schema_error)?;
        validate_pds_projection_contract(&self.connection)?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let target = ReadTarget {
            revision: current_revision(&transaction)?,
            generation: active_generation(&transaction)?,
        };
        nonnegative_u64(target.revision, "source revision")?;
        let active_pds_generation = super::active_generation(&transaction)?;
        let active_revision = super::current_revision(&transaction)?;
        if target.generation != active_pds_generation || target.revision != active_revision {
            return validation(
                "logical head initialization requires the current active PDS revision",
            );
        }
        let already_exists: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM logical_sync_generations
                WHERE library_id = ?1 AND generation_id = ?2
             )",
            params![request.library_id, request.generation_id],
            |row| row.get(0),
        )?;
        if already_exists {
            return validation("logical generation index already exists");
        }
        if let Some(parent_generation_id) = &request.parent_generation_id {
            let parent_complete: bool = transaction.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM logical_sync_generations
                    WHERE library_id = ?1 AND generation_id = ?2 AND state = 'complete'
                 )",
                params![request.library_id, parent_generation_id],
                |row| row.get(0),
            )?;
            if !parent_complete {
                return validation("logical parent generation is absent or incomplete");
            }
        }

        let created_at = unix_millis()?;
        transaction.execute(
            "INSERT INTO logical_sync_generations (
                library_id, generation_id, generation_sequence, parent_generation_id,
                pds_generation, source_revision, state, manifest_hash, created_at, completed_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'building', NULL, ?7, NULL)",
            params![
                request.library_id,
                request.generation_id,
                request.generation_sequence,
                request.parent_generation_id,
                target.generation,
                target.revision,
                created_at,
            ],
        )?;

        project_root(
            &transaction,
            cas,
            &request.library_id,
            &request.generation_id,
            &target.generation,
        )?;
        project_presets(
            &transaction,
            &request.library_id,
            &request.generation_id,
            &target.generation,
        )?;
        project_plugins(
            &transaction,
            &request.library_id,
            &request.generation_id,
            &target.generation,
        )?;
        project_characters(
            &transaction,
            cas,
            &request.library_id,
            &request.generation_id,
            &target.generation,
        )?;
        project_conversations(
            &transaction,
            &request.library_id,
            &request.generation_id,
            &target.generation,
        )?;
        project_asset_aliases(
            &transaction,
            cas,
            &request.library_id,
            &request.generation_id,
            &target.generation,
        )?;
        project_cold_aliases(
            &transaction,
            cas,
            &request.library_id,
            &request.generation_id,
            &target.generation,
        )?;
        if let Some(parent_generation_id) = &request.parent_generation_id {
            project_tombstones(
                &transaction,
                &request.library_id,
                &request.generation_id,
                parent_generation_id,
                &request.generation_sequence,
            )?;
        }

        transaction.execute(
            "INSERT INTO logical_library_head (singleton, library_id, generation_id)
             VALUES (1, ?1, ?2)
             ON CONFLICT(singleton) DO UPDATE SET
                library_id = excluded.library_id,
                generation_id = excluded.generation_id",
            params![request.library_id, request.generation_id,],
        )?;
        logical_schema::validate_logical_schema(&transaction).map_err(schema_error)?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn seal_active_logical_generation(
        &mut self,
        cas: &PayloadCas,
    ) -> StoreResult<BuiltIndexedLogicalManifest> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (library_id, generation_id): (String, String) = transaction
            .query_row(
                "SELECT library_id, generation_id
                 FROM logical_library_head WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| StoreError::Validation {
                message: "logical library head is not initialized".to_owned(),
            })?;
        let (pds_generation, source_revision, state, created_at): (String, i64, String, i64) =
            transaction.query_row(
                "SELECT pds_generation, source_revision, state, created_at
                 FROM logical_sync_generations
                 WHERE library_id = ?1 AND generation_id = ?2",
                params![library_id, generation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        let active = super::active_generation(&transaction)?;
        let revision = super::current_revision(&transaction)?;
        if pds_generation != active || source_revision != revision {
            return validation("logical library head is not current with the active PDS revision");
        }

        if state == "complete" {
            let built = scan_compact_manifest(&transaction, &library_id, &generation_id, true)?;
            transaction.commit()?;
            return Ok(built);
        }
        if state != "building" {
            return validation("logical library head has an unsupported state");
        }
        let built = scan_compact_manifest(&transaction, &library_id, &generation_id, false)?;
        let prepared_manifest = cas.prepare_bytes(&built.manifest_bytes)?;
        if prepared_manifest.content_hash != built.manifest_hash
            || prepared_manifest.byte_size != built.manifest_bytes.len() as u64
        {
            return validation(
                "prepared logical manifest object does not match its canonical bytes",
            );
        }
        let completed_at = unix_millis()?.max(created_at);
        let changed = transaction.execute(
            "UPDATE logical_sync_generations
             SET state = 'complete', manifest_hash = ?3, completed_at = ?4
             WHERE library_id = ?1 AND generation_id = ?2 AND state = 'building'",
            params![library_id, generation_id, built.manifest_hash, completed_at],
        )?;
        if changed != 1 {
            return validation("logical generation seal lost its building state");
        }
        logical_schema::validate_logical_schema(&transaction).map_err(schema_error)?;
        transaction.commit()?;
        Ok(built)
    }

    pub(crate) fn pin_logical_generation(
        &mut self,
        library_id: &str,
        generation_id: &str,
    ) -> StoreResult<String> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let complete: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM logical_sync_generations
                WHERE library_id = ?1 AND generation_id = ?2 AND state = 'complete'
             )",
            params![library_id, generation_id],
            |row| row.get(0),
        )?;
        if !complete {
            return validation("only a complete logical generation can be session-pinned");
        }
        let session_id = format!("logical-session-{}", uuid::Uuid::new_v4());
        transaction.execute(
            "INSERT INTO logical_generation_session_pins (
                session_id, library_id, generation_id, created_at
             ) VALUES (?1, ?2, ?3, ?4)",
            params![session_id, library_id, generation_id, unix_millis()?],
        )?;
        transaction.commit()?;
        Ok(session_id)
    }

    pub(crate) fn resume_logical_generation_pin(
        &mut self,
        session_id: &str,
        library_id: &str,
        generation_id: &str,
    ) -> StoreResult<()> {
        if !session_id.starts_with("logical-session-") {
            return validation("logical generation session pin has an invalid ID");
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let tuple: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT pin.library_id, pin.generation_id, generation.state
                 FROM logical_generation_session_pins AS pin
                 JOIN logical_sync_generations AS generation
                   ON generation.library_id = pin.library_id
                  AND generation.generation_id = pin.generation_id
                 WHERE pin.session_id = ?1",
                [session_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((pinned_library_id, pinned_generation_id, state)) = tuple else {
            return validation("logical generation session pin does not exist");
        };
        if pinned_library_id != library_id || pinned_generation_id != generation_id {
            return validation("logical generation session pin tuple does not match");
        }
        if state != "complete" {
            return validation("logical generation session pin is not complete");
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn release_logical_generation_pin(&mut self, session_id: &str) -> StoreResult<()> {
        if !session_id.starts_with("logical-session-") {
            return validation("logical generation session pin has an invalid ID");
        }
        self.connection.execute(
            "DELETE FROM logical_generation_session_pins WHERE session_id = ?1",
            [session_id],
        )?;
        Ok(())
    }

    pub(crate) fn prune_logical_generation(
        &mut self,
        library_id: &str,
        generation_id: &str,
    ) -> StoreResult<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pds_generation: String = transaction
            .query_row(
                "SELECT pds_generation FROM logical_sync_generations
                 WHERE library_id = ?1 AND generation_id = ?2",
                params![library_id, generation_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::Validation {
                message: "logical generation index does not exist".to_owned(),
            })?;
        let is_head: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM logical_library_head
                WHERE library_id = ?1 AND generation_id = ?2
             )",
            params![library_id, generation_id],
            |row| row.get(0),
        )?;
        if is_head {
            return validation("logical library head cannot be pruned");
        }
        let session_pinned: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM logical_generation_session_pins
                WHERE library_id = ?1 AND generation_id = ?2
             )",
            params![library_id, generation_id],
            |row| row.get(0),
        )?;
        if session_pinned {
            return validation("session-pinned logical generation cannot be pruned");
        }
        delete_logical_rows(
            &transaction,
            library_id,
            generation_id,
            LogicalDeleteScope::Generation,
        )?;
        let active = super::active_generation(&transaction)?;
        if pds_generation != active
            && !super::generation_is_retained(&transaction, &pds_generation)?
        {
            super::commit::delete_generation(&transaction, &pds_generation)?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn build_indexed_logical_manifest(
        &self,
        library_id: &str,
        generation_id: &str,
    ) -> StoreResult<BuiltIndexedLogicalManifest> {
        scan_compact_manifest(&self.connection, library_id, generation_id, true)
    }

    pub(crate) fn reconstruct_logical_object(
        &self,
        cas: &PayloadCas,
        library_id: &str,
        generation_id: &str,
        object_hash: &str,
    ) -> StoreResult<Vec<u8>> {
        let pds_generation =
            require_complete_generation(&self.connection, library_id, generation_id)?;
        let expected_size =
            referenced_object_size(&self.connection, library_id, generation_id, object_hash)?;

        let record_key: Option<String> = self
            .connection
            .query_row(
                "SELECT record_key FROM logical_record_heads
                 WHERE library_id = ?1 AND generation_id = ?2
                   AND state = 'live' AND object_hash = ?3
                 ORDER BY record_key ASC LIMIT 1",
                params![library_id, generation_id, object_hash],
                |row| row.get(0),
            )
            .optional()?;
        let bytes = if let Some(record_key) = record_key {
            reconstruct_record(
                &self.connection,
                cas,
                library_id,
                generation_id,
                &pds_generation,
                &record_key,
            )?
        } else {
            let page: Option<(String, i64, i64)> = self
                .connection
                .query_row(
                    "SELECT record_key, first_message_index, message_count
                     FROM logical_message_page_sources
                     WHERE library_id = ?1 AND generation_id = ?2 AND object_hash = ?3
                     ORDER BY record_key ASC, page_index ASC LIMIT 1",
                    params![library_id, generation_id, object_hash],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((record_key, first_message_index, message_count)) = page {
                reconstruct_page(
                    &self.connection,
                    &pds_generation,
                    &record_key,
                    nonnegative_u64(first_message_index, "message page first index")?,
                    nonnegative_u64(message_count, "message page count")?,
                )?
            } else {
                cas.read_object(object_hash)?
                    .ok_or_else(|| StoreError::Validation {
                        message: format!("referenced CAS object {object_hash} is missing"),
                    })?
            }
        };
        verify_object_bytes(&bytes, object_hash, expected_size)?;
        Ok(bytes)
    }
}

pub(super) fn begin_incremental_logical_commit(
    transaction: &Transaction<'_>,
    source_pds_generation: &str,
    target_pds_generation: &str,
    source_revision: i64,
) -> StoreResult<Option<IncrementalLogicalCommit>> {
    if !logical_index_is_active(transaction)? {
        return Ok(None);
    }
    let (library_id, generation_id, generation_sequence, state, indexed_pds): (
        String,
        String,
        String,
        String,
        String,
    ) = transaction.query_row(
        "SELECT generation.library_id, generation.generation_id,
                generation.generation_sequence, generation.state, generation.pds_generation
         FROM logical_library_head AS head
         JOIN logical_sync_generations AS generation
           ON generation.library_id = head.library_id
          AND generation.generation_id = head.generation_id
         WHERE head.singleton = 1",
        [],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    if indexed_pds != source_pds_generation {
        return validation("logical library head does not reference the active PDS generation");
    }

    if state == "building" {
        transaction.execute(
            "UPDATE logical_sync_generations
             SET pds_generation = ?3, source_revision = ?4
             WHERE library_id = ?1 AND generation_id = ?2 AND state = 'building'",
            params![
                library_id,
                generation_id,
                target_pds_generation,
                source_revision,
            ],
        )?;
        return Ok(Some(IncrementalLogicalCommit {
            library_id,
            generation_id,
            generation_sequence,
            pds_generation: target_pds_generation.to_owned(),
        }));
    }
    if state != "complete" {
        return validation("logical library head has an unsupported state");
    }
    if source_pds_generation == target_pds_generation {
        return validation("a complete logical generation must use PDS copy-on-write");
    }

    let child_generation_id = format!("logical-generation-{}", uuid::Uuid::new_v4());
    let child_sequence = increment_decimal(&generation_sequence)?;
    transaction.execute(
        "INSERT INTO logical_sync_generations (
            library_id, generation_id, generation_sequence, parent_generation_id,
            pds_generation, source_revision, state, manifest_hash, created_at, completed_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'building', NULL, ?7, NULL)",
        params![
            library_id,
            child_generation_id,
            child_sequence,
            generation_id,
            target_pds_generation,
            source_revision,
            unix_millis()?,
        ],
    )?;
    transaction.execute(
        "INSERT INTO logical_record_heads (
            library_id, generation_id, record_key, record_kind, state,
            object_hash, object_size, deleted_generation_sequence
         )
         SELECT library_id, ?3, record_key, record_kind, state,
                object_hash, object_size, deleted_generation_sequence
         FROM logical_record_heads
         WHERE library_id = ?1 AND generation_id = ?2",
        params![library_id, generation_id, child_generation_id],
    )?;
    transaction.execute(
        "INSERT INTO logical_record_dependencies (
            library_id, generation_id, record_key, object_hash, object_size
         )
         SELECT library_id, ?3, record_key, object_hash, object_size
         FROM logical_record_dependencies
         WHERE library_id = ?1 AND generation_id = ?2",
        params![library_id, generation_id, child_generation_id],
    )?;
    transaction.execute(
        "INSERT INTO logical_message_page_sources (
            library_id, generation_id, record_key, record_kind, page_index,
            first_message_index, message_count, object_hash, object_size
         )
         SELECT library_id, ?3, record_key, record_kind, page_index,
                first_message_index, message_count, object_hash, object_size
         FROM logical_message_page_sources
         WHERE library_id = ?1 AND generation_id = ?2",
        params![library_id, generation_id, child_generation_id],
    )?;
    transaction.execute(
        "UPDATE logical_library_head SET generation_id = ?2
         WHERE singleton = 1 AND library_id = ?1",
        params![library_id, child_generation_id],
    )?;
    Ok(Some(IncrementalLogicalCommit {
        library_id,
        generation_id: child_generation_id,
        generation_sequence: child_sequence,
        pds_generation: target_pds_generation.to_owned(),
    }))
}

pub(super) fn detach_logical_head_for_full_replace(
    transaction: &Transaction<'_>,
    active_pds_generation: &str,
) -> StoreResult<()> {
    if !logical_index_is_active(transaction)? {
        return Ok(());
    }
    let (library_id, generation_id, pds_generation, state): (String, String, String, String) =
        transaction.query_row(
            "SELECT generation.library_id, generation.generation_id,
                    generation.pds_generation, generation.state
             FROM logical_library_head AS head
             JOIN logical_sync_generations AS generation
               ON generation.library_id = head.library_id
              AND generation.generation_id = head.generation_id
             WHERE head.singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    if pds_generation != active_pds_generation {
        return validation("logical library head does not reference the replaced PDS generation");
    }
    transaction.execute("DELETE FROM logical_library_head WHERE singleton = 1", [])?;
    if state == "building" {
        delete_logical_rows(
            transaction,
            &library_id,
            &generation_id,
            LogicalDeleteScope::Generation,
        )?;
    }
    Ok(())
}

fn increment_decimal(value: &str) -> StoreResult<String> {
    if value.is_empty()
        || value.bytes().any(|byte| !byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return validation("logical generation sequence is not canonical decimal");
    }
    let mut bytes = value.as_bytes().to_vec();
    let mut index = bytes.len();
    while index > 0 {
        index -= 1;
        if bytes[index] != b'9' {
            bytes[index] += 1;
            return String::from_utf8(bytes).map_err(|_| StoreError::Validation {
                message: "logical generation sequence is not ASCII".to_owned(),
            });
        }
        bytes[index] = b'0';
    }
    let mut result = Vec::with_capacity(bytes.len() + 1);
    result.push(b'1');
    result.extend(bytes);
    String::from_utf8(result).map_err(|_| StoreError::Validation {
        message: "logical generation sequence is not ASCII".to_owned(),
    })
}

pub(super) fn maintain_incremental_logical_commit(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    logical: &IncrementalLogicalCommit,
    input: &WorkingSetCommit,
    conversation_changes: &[ConversationChange],
    shifted_conversation_indices: &BTreeMap<String, u64>,
    prior_character_conversations: &BTreeMap<String, Vec<String>>,
) -> StoreResult<()> {
    if input.root.is_some() {
        replace_root_projection(transaction, cas, logical)?;
    }
    if input.replace_presets.is_some() {
        replace_kind_projection(transaction, logical, RECORD_KIND_PRESET)?;
        project_presets(
            transaction,
            &logical.library_id,
            &logical.generation_id,
            &logical.pds_generation,
        )?;
        restore_parent_tombstones_for_kind(transaction, logical, RECORD_KIND_PRESET)?;
    }

    let mut full_characters = Vec::new();
    if let Some(character_id) = input.delete_character_id.as_deref() {
        delete_character_projection(
            transaction,
            logical,
            character_id,
            prior_character_conversations
                .get(character_id)
                .map(Vec::as_slice)
                .unwrap_or_default(),
        )?;
    }
    for character in input
        .replace_character
        .iter()
        .chain(input.add_character.iter())
    {
        if let Some(character_id) = character.get("chaId").and_then(Value::as_str) {
            refresh_full_character_projection(
                transaction,
                cas,
                logical,
                character_id,
                prior_character_conversations
                    .get(character_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default(),
            )?;
            full_characters.push(character_id.to_owned());
        }
    }
    for character in input
        .character
        .iter()
        .chain(input.character_details.iter().flatten())
    {
        if let Some(character_id) = character.get("chaId").and_then(Value::as_str) {
            replace_character_projection(transaction, cas, logical, character_id)?;
        }
    }

    for change in conversation_changes {
        let character_id = match change {
            ConversationChange::Delete { character_id, .. }
            | ConversationChange::ReplaceRange { character_id, .. } => character_id,
        };
        if full_characters.iter().any(|full| full == character_id) {
            continue;
        }
        maintain_conversation_projection(transaction, logical, change)?;
    }
    for (character_id, configured_index) in shifted_conversation_indices {
        if full_characters.iter().any(|full| full == character_id) {
            continue;
        }
        refresh_shifted_conversation_envelopes(
            transaction,
            logical,
            character_id,
            *configured_index,
            conversation_changes,
        )?;
    }

    for mutation in input.plugin_storage.as_deref().unwrap_or_default() {
        match mutation {
            PluginStorageMutation::Set { key, .. } => {
                replace_plugin_projection(transaction, logical, key)?;
            }
            PluginStorageMutation::Delete { key } => {
                delete_projection(
                    transaction,
                    logical,
                    encode_logical_record_key(&LogicalRecordLocator::Plugin {
                        storage_key: key.clone(),
                    })
                    .map_err(codec_error)?,
                    RECORD_KIND_PLUGIN,
                )?;
            }
            PluginStorageMutation::Clear => {
                replace_kind_projection(transaction, logical, RECORD_KIND_PLUGIN)?;
                restore_parent_tombstones_for_kind(transaction, logical, RECORD_KIND_PLUGIN)?;
            }
        }
    }

    Ok(())
}

fn refresh_shifted_conversation_envelopes(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    character_id: &str,
    configured_index: u64,
    primary_changes: &[ConversationChange],
) -> StoreResult<()> {
    let conversations = {
        let mut statement = transaction.prepare(
            "SELECT conversation_id, configured_index, recent_at, detail, message_count
             FROM conversations
             WHERE generation = ?1 AND character_id = ?2 AND configured_index >= ?3
             ORDER BY configured_index ASC, conversation_id ASC",
        )?;
        let rows = statement.query_map(
            params![
                logical.pds_generation,
                character_id,
                sqlite_i64(configured_index, "shifted conversation configured index")?,
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )?;
        let conversations = rows.collect::<Result<Vec<_>, _>>()?;
        conversations
    };
    for (conversation_id, configured_index, recent_at, detail, message_count) in conversations {
        if primary_changes.iter().any(|change| {
            let (changed_character_id, changed_conversation_id) =
                conversation_change_identity(change);
            changed_character_id == character_id && changed_conversation_id == conversation_id
        }) {
            continue;
        }
        let key = encode_logical_record_key(&LogicalRecordLocator::Conversation {
            character_id: character_id.to_owned(),
            conversation_id,
        })
        .map_err(codec_error)?;
        rebuild_conversation_head(
            transaction,
            logical,
            &key,
            configured_index,
            recent_at,
            serde_json::from_str(&detail)?,
            nonnegative_u64(message_count, "conversation message count")?,
            false,
        )?;
    }
    Ok(())
}

pub(super) fn maintain_incremental_asset_alias(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    logical: &IncrementalLogicalCommit,
    alias: &AssetAlias,
) -> StoreResult<()> {
    let locator = match alias.kind.as_str() {
        RECORD_KIND_ASSET => LogicalRecordLocator::Asset {
            logical_key: alias.key.clone(),
        },
        RECORD_KIND_INLAY => LogicalRecordLocator::Inlay {
            logical_key: alias.key.clone(),
        },
        _ => return validation("asset alias kind is unsupported"),
    };
    let key = encode_logical_record_key(&locator).map_err(codec_error)?;
    delete_head(transaction, logical, &key)?;
    project_asset_alias_by_key(transaction, cas, logical, &alias.kind, &alias.key)?;
    Ok(())
}

pub(super) fn maintain_incremental_asset_alias_deletion(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    kind: &str,
    logical_key: &str,
) -> StoreResult<()> {
    let locator = match kind {
        RECORD_KIND_ASSET => LogicalRecordLocator::Asset {
            logical_key: logical_key.to_owned(),
        },
        RECORD_KIND_INLAY => LogicalRecordLocator::Inlay {
            logical_key: logical_key.to_owned(),
        },
        _ => return validation("asset alias kind is unsupported"),
    };
    delete_projection(
        transaction,
        logical,
        encode_logical_record_key(&locator).map_err(codec_error)?,
        kind,
    )
}

pub(super) fn maintain_incremental_cold_alias(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    logical: &IncrementalLogicalCommit,
    logical_key: &str,
) -> StoreResult<()> {
    let locator = LogicalRecordLocator::Cold {
        logical_key: logical_key.to_owned(),
    };
    let key = encode_logical_record_key(&locator).map_err(codec_error)?;
    delete_head(transaction, logical, &key)?;
    project_cold_alias_by_key(transaction, cas, logical, logical_key)
}

pub(super) fn maintain_incremental_cold_alias_deletion(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    logical_key: &str,
) -> StoreResult<()> {
    delete_projection(
        transaction,
        logical,
        encode_logical_record_key(&LogicalRecordLocator::Cold {
            logical_key: logical_key.to_owned(),
        })
        .map_err(codec_error)?,
        RECORD_KIND_COLD,
    )
}

pub(super) fn maintain_incremental_cold_alias_replacement(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    logical: &IncrementalLogicalCommit,
) -> StoreResult<()> {
    replace_kind_projection(transaction, logical, RECORD_KIND_COLD)?;
    project_cold_aliases(
        transaction,
        cas,
        &logical.library_id,
        &logical.generation_id,
        &logical.pds_generation,
    )?;
    restore_parent_tombstones_for_kind(transaction, logical, RECORD_KIND_COLD)
}

fn replace_root_projection(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    logical: &IncrementalLogicalCommit,
) -> StoreResult<()> {
    let key = encode_logical_record_key(&LogicalRecordLocator::Root).map_err(codec_error)?;
    delete_head(transaction, logical, &key)?;
    project_root(
        transaction,
        cas,
        &logical.library_id,
        &logical.generation_id,
        &logical.pds_generation,
    )
}

fn replace_kind_projection(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    record_kind: &str,
) -> StoreResult<()> {
    delete_logical_rows(
        transaction,
        &logical.library_id,
        &logical.generation_id,
        LogicalDeleteScope::RecordKind(record_kind),
    )
}

fn delete_head(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    record_key: &str,
) -> StoreResult<()> {
    delete_logical_rows(
        transaction,
        &logical.library_id,
        &logical.generation_id,
        LogicalDeleteScope::RecordKey(record_key),
    )
}

fn delete_logical_rows(
    transaction: &Transaction<'_>,
    library_id: &str,
    generation_id: &str,
    scope: LogicalDeleteScope<'_>,
) -> StoreResult<()> {
    let (record_key, record_kind) = match scope {
        LogicalDeleteScope::RecordKey(record_key) => (Some(record_key), None),
        LogicalDeleteScope::RecordKind(record_kind) => (None, Some(record_kind)),
        LogicalDeleteScope::Generation => (None, None),
    };
    transaction.execute(
        "DELETE FROM logical_record_dependencies
         WHERE library_id = ?1 AND generation_id = ?2
           AND (?3 IS NULL OR record_key = ?3)
           AND (
               ?4 IS NULL OR EXISTS (
                   SELECT 1 FROM logical_record_heads AS head
                   WHERE head.library_id = logical_record_dependencies.library_id
                     AND head.generation_id = logical_record_dependencies.generation_id
                     AND head.record_key = logical_record_dependencies.record_key
                     AND head.record_kind = ?4
               )
           )",
        params![library_id, generation_id, record_key, record_kind],
    )?;
    transaction.execute(
        "DELETE FROM logical_message_page_sources
         WHERE library_id = ?1 AND generation_id = ?2
           AND (?3 IS NULL OR record_key = ?3)
           AND (?4 IS NULL OR record_kind = ?4)",
        params![library_id, generation_id, record_key, record_kind],
    )?;
    transaction.execute(
        "DELETE FROM logical_record_heads
         WHERE library_id = ?1 AND generation_id = ?2
           AND (?3 IS NULL OR record_key = ?3)
           AND (?4 IS NULL OR record_kind = ?4)",
        params![library_id, generation_id, record_key, record_kind],
    )?;
    if matches!(scope, LogicalDeleteScope::Generation) {
        transaction.execute(
            "DELETE FROM logical_sync_generations
             WHERE library_id = ?1 AND generation_id = ?2",
            params![library_id, generation_id],
        )?;
    }
    Ok(())
}

fn delete_projection(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    record_key: String,
    record_kind: &str,
) -> StoreResult<()> {
    delete_head(transaction, logical, &record_key)?;
    let parent: Option<(String, Option<String>)> = transaction
        .query_row(
            "SELECT parent.state, parent.deleted_generation_sequence
             FROM logical_sync_generations AS current
             JOIN logical_record_heads AS parent
               ON parent.library_id = current.library_id
              AND parent.generation_id = current.parent_generation_id
              AND parent.record_key = ?3
             WHERE current.library_id = ?1 AND current.generation_id = ?2",
            params![logical.library_id, logical.generation_id, record_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((parent_state, parent_deleted_sequence)) = parent else {
        return Ok(());
    };
    let deleted_sequence = if parent_state == "tombstone" {
        parent_deleted_sequence.ok_or_else(|| StoreError::Validation {
            message: "parent logical tombstone is missing its deletion sequence".to_owned(),
        })?
    } else {
        logical.generation_sequence.clone()
    };
    transaction.execute(
        "INSERT INTO logical_record_heads (
            library_id, generation_id, record_key, record_kind, state,
            object_hash, object_size, deleted_generation_sequence
         ) VALUES (?1, ?2, ?3, ?4, 'tombstone', NULL, 0, ?5)",
        params![
            logical.library_id,
            logical.generation_id,
            record_key,
            record_kind,
            deleted_sequence,
        ],
    )?;
    Ok(())
}

fn restore_parent_tombstones_for_kind(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    record_kind: &str,
) -> StoreResult<()> {
    let parent: Option<String> = transaction.query_row(
        "SELECT parent_generation_id FROM logical_sync_generations
         WHERE library_id = ?1 AND generation_id = ?2",
        params![logical.library_id, logical.generation_id],
        |row| row.get(0),
    )?;
    if let Some(parent) = parent {
        transaction.execute(
            "INSERT INTO logical_record_heads (
                library_id, generation_id, record_key, record_kind, state,
                object_hash, object_size, deleted_generation_sequence
             )
             SELECT parent.library_id, ?2, parent.record_key, parent.record_kind, 'tombstone',
                    NULL, 0,
                    CASE WHEN parent.state = 'tombstone'
                         THEN parent.deleted_generation_sequence ELSE ?5 END
             FROM logical_record_heads AS parent
             WHERE parent.library_id = ?1 AND parent.generation_id = ?3
               AND parent.record_kind = ?4
               AND NOT EXISTS (
                   SELECT 1 FROM logical_record_heads AS current
                   WHERE current.library_id = ?1 AND current.generation_id = ?2
                     AND current.record_key = parent.record_key
               )",
            params![
                logical.library_id,
                logical.generation_id,
                parent,
                record_kind,
                logical.generation_sequence,
            ],
        )?;
    }
    Ok(())
}

fn replace_plugin_projection(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    storage_key: &str,
) -> StoreResult<()> {
    let key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
        storage_key: storage_key.to_owned(),
    })
    .map_err(codec_error)?;
    delete_head(transaction, logical, &key)?;
    let row: Option<(i64, String)> = transaction
        .query_row(
            "SELECT ordinal, value FROM plugin_storage
             WHERE generation = ?1 AND storage_key = ?2",
            params![logical.pds_generation, storage_key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((ordinal, raw)) = row {
        insert_live_record(
            transaction,
            &logical.library_id,
            &logical.generation_id,
            key,
            RECORD_KIND_PLUGIN,
            LogicalRecordEnvelope::Plugin {
                ordinal: nonnegative_u64(ordinal, "plugin storage ordinal")?,
                value: serde_json::from_str(&raw)?,
            },
            Vec::new(),
            &[],
        )?;
    }
    Ok(())
}

fn replace_character_projection(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    logical: &IncrementalLogicalCommit,
    character_id: &str,
) -> StoreResult<()> {
    let key = encode_logical_record_key(&LogicalRecordLocator::Character {
        character_id: character_id.to_owned(),
    })
    .map_err(codec_error)?;
    delete_head(transaction, logical, &key)?;
    if !project_character_by_id(transaction, cas, logical, character_id)? {
        return validation("changed character is missing from the active PDS generation");
    }
    Ok(())
}

fn project_character_by_id(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    logical: &IncrementalLogicalCommit,
    character_id: &str,
) -> StoreResult<bool> {
    let row: Option<(i64, String)> = transaction
        .query_row(
            "SELECT configured_index, detail FROM characters
             WHERE generation = ?1 AND character_id = ?2",
            params![logical.pds_generation, character_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((configured_index, raw)) = row else {
        return Ok(false);
    };
    let mut owner_heads = validate_owner_heads(
        cas,
        load_owner_heads(transaction, &logical.pds_generation, Some(character_id))?,
    )?;
    let mut detail: Value = serde_json::from_str(&raw)?;
    strip_character_owner_property(&mut detail, character_id, &mut owner_heads)?;
    let dependencies = owner_dependencies(&owner_heads)?;
    insert_live_record(
        transaction,
        &logical.library_id,
        &logical.generation_id,
        encode_logical_record_key(&LogicalRecordLocator::Character {
            character_id: character_id.to_owned(),
        })
        .map_err(codec_error)?,
        RECORD_KIND_CHARACTER,
        LogicalRecordEnvelope::Character {
            configured_index: nonnegative_u64(configured_index, "character configured index")?,
            detail,
            owner_heads: logical_owner_heads(&owner_heads),
        },
        dependencies,
        &[],
    )?;
    Ok(true)
}

fn delete_character_projection(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    character_id: &str,
    conversation_ids: &[String],
) -> StoreResult<()> {
    let key = encode_logical_record_key(&LogicalRecordLocator::Character {
        character_id: character_id.to_owned(),
    })
    .map_err(codec_error)?;
    delete_projection(transaction, logical, key, RECORD_KIND_CHARACTER)?;
    for conversation_id in conversation_ids {
        delete_projection(
            transaction,
            logical,
            encode_logical_record_key(&LogicalRecordLocator::Conversation {
                character_id: character_id.to_owned(),
                conversation_id: conversation_id.clone(),
            })
            .map_err(codec_error)?,
            RECORD_KIND_CONVERSATION,
        )?;
    }
    Ok(())
}

fn refresh_full_character_projection(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    logical: &IncrementalLogicalCommit,
    character_id: &str,
    prior_conversation_ids: &[String],
) -> StoreResult<()> {
    let key = encode_logical_record_key(&LogicalRecordLocator::Character {
        character_id: character_id.to_owned(),
    })
    .map_err(codec_error)?;
    delete_head(transaction, logical, &key)?;
    for conversation_id in prior_conversation_ids {
        delete_projection(
            transaction,
            logical,
            encode_logical_record_key(&LogicalRecordLocator::Conversation {
                character_id: character_id.to_owned(),
                conversation_id: conversation_id.clone(),
            })
            .map_err(codec_error)?,
            RECORD_KIND_CONVERSATION,
        )?;
    }
    if !project_character_by_id(transaction, cas, logical, character_id)? {
        return validation("replaced character is missing from the active PDS generation");
    }
    let mut statement = transaction.prepare(
        "SELECT conversation_id FROM conversations
         WHERE generation = ?1 AND character_id = ?2 ORDER BY conversation_id ASC",
    )?;
    let conversations = statement
        .query_map(params![logical.pds_generation, character_id], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for conversation_id in conversations {
        let key = encode_logical_record_key(&LogicalRecordLocator::Conversation {
            character_id: character_id.to_owned(),
            conversation_id: conversation_id.clone(),
        })
        .map_err(codec_error)?;
        delete_head(transaction, logical, &key)?;
        project_conversation_full(transaction, logical, character_id, &conversation_id)?;
    }
    Ok(())
}

fn maintain_conversation_projection(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    change: &ConversationChange,
) -> StoreResult<()> {
    match change {
        ConversationChange::Delete {
            character_id,
            conversation_id,
        } => delete_projection(
            transaction,
            logical,
            encode_logical_record_key(&LogicalRecordLocator::Conversation {
                character_id: character_id.clone(),
                conversation_id: conversation_id.clone(),
            })
            .map_err(codec_error)?,
            RECORD_KIND_CONVERSATION,
        ),
        ConversationChange::ReplaceRange {
            character_id,
            conversation_id,
            old_count,
            new_count,
            fixed_page_ranges,
            tail_from_page,
            was_new,
        } => refresh_conversation_range(
            transaction,
            logical,
            character_id,
            conversation_id,
            *old_count,
            *new_count,
            fixed_page_ranges,
            *tail_from_page,
            *was_new,
        ),
    }
}

fn project_conversation_full(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    character_id: &str,
    conversation_id: &str,
) -> StoreResult<()> {
    let (configured_index, recent_at, raw, expected_count): (i64, i64, String, i64) = transaction
        .query_row(
        "SELECT configured_index, recent_at, detail, message_count
             FROM conversations
             WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
        params![logical.pds_generation, character_id, conversation_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let key = encode_logical_record_key(&LogicalRecordLocator::Conversation {
        character_id: character_id.to_owned(),
        conversation_id: conversation_id.to_owned(),
    })
    .map_err(codec_error)?;
    let pages = project_message_pages(
        transaction,
        &logical.pds_generation,
        character_id,
        conversation_id,
        nonnegative_u64(expected_count, "conversation message count")?,
    )?;
    let dependencies = pages.iter().map(|page| page.object.clone()).collect();
    let message_page_hashes = pages.iter().map(|page| page.object.hash.clone()).collect();
    insert_live_record(
        transaction,
        &logical.library_id,
        &logical.generation_id,
        key,
        RECORD_KIND_CONVERSATION,
        LogicalRecordEnvelope::Conversation {
            configured_index: nonnegative_u64(configured_index, "conversation configured index")?,
            recent_at,
            detail: serde_json::from_str(&raw)?,
            message_page_hashes,
        },
        dependencies,
        &pages,
    )
}

#[allow(clippy::too_many_arguments)]
fn refresh_conversation_range(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    character_id: &str,
    conversation_id: &str,
    old_count: u64,
    new_count: u64,
    fixed_page_ranges: &[(u64, u64)],
    tail_from_page: Option<u64>,
    was_new: bool,
) -> StoreResult<()> {
    if old_count != new_count && tail_from_page.is_none() {
        return validation("conversation length change is missing a tail page range");
    }
    let key = encode_logical_record_key(&LogicalRecordLocator::Conversation {
        character_id: character_id.to_owned(),
        conversation_id: conversation_id.to_owned(),
    })
    .map_err(codec_error)?;
    let existing_live: bool = transaction.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM logical_record_heads
            WHERE library_id = ?1 AND generation_id = ?2 AND record_key = ?3 AND state = 'live'
         )",
        params![logical.library_id, logical.generation_id, key],
        |row| row.get(0),
    )?;
    if was_new || !existing_live {
        delete_head(transaction, logical, &key)?;
        return project_conversation_full(transaction, logical, character_id, conversation_id);
    }

    let (configured_index, recent_at, raw, stored_count): (i64, i64, String, i64) = transaction
        .query_row(
            "SELECT configured_index, recent_at, detail, message_count
             FROM conversations
             WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
            params![logical.pds_generation, character_id, conversation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
    if nonnegative_u64(stored_count, "conversation message count")? != new_count {
        return validation("conversation mutation result count does not match PDS metadata");
    }

    transaction.execute(
        "DELETE FROM logical_record_dependencies
         WHERE library_id = ?1 AND generation_id = ?2 AND record_key = ?3",
        params![logical.library_id, logical.generation_id, key],
    )?;
    for &(start_page, end_page) in fixed_page_ranges {
        transaction.execute(
            "DELETE FROM logical_message_page_sources
             WHERE library_id = ?1 AND generation_id = ?2 AND record_key = ?3
               AND page_index >= ?4 AND page_index < ?5",
            params![
                logical.library_id,
                logical.generation_id,
                key,
                sqlite_i64(start_page, "first fixed message page")?,
                sqlite_i64(end_page, "fixed message page end")?,
            ],
        )?;
        for page_index in start_page..end_page {
            insert_message_page_source(
                transaction,
                logical,
                &key,
                character_id,
                conversation_id,
                page_index,
                new_count,
            )?;
        }
    }
    if let Some(first_page) = tail_from_page {
        transaction.execute(
            "DELETE FROM logical_message_page_sources
             WHERE library_id = ?1 AND generation_id = ?2 AND record_key = ?3
               AND page_index >= ?4",
            params![
                logical.library_id,
                logical.generation_id,
                key,
                sqlite_i64(first_page, "first tail message page")?,
            ],
        )?;
        let page_count = new_count.div_ceil(LOGICAL_MESSAGE_PAGE_SIZE as u64);
        for page_index in first_page..page_count {
            insert_message_page_source(
                transaction,
                logical,
                &key,
                character_id,
                conversation_id,
                page_index,
                new_count,
            )?;
        }
    }
    rebuild_conversation_head(
        transaction,
        logical,
        &key,
        configured_index,
        recent_at,
        serde_json::from_str(&raw)?,
        new_count,
        true,
    )
}

fn insert_message_page_source(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    record_key: &str,
    character_id: &str,
    conversation_id: &str,
    page_index: u64,
    message_count: u64,
) -> StoreResult<()> {
    let first_message_index = page_index
        .checked_mul(LOGICAL_MESSAGE_PAGE_SIZE as u64)
        .ok_or_else(|| StoreError::Validation {
            message: "conversation page first index overflow".to_owned(),
        })?;
    if first_message_index >= message_count {
        return validation("conversation page starts beyond the final message count");
    }
    #[cfg(test)]
    record_message_page_rehash_read(character_id, conversation_id, page_index);
    let end = first_message_index
        .saturating_add(LOGICAL_MESSAGE_PAGE_SIZE as u64)
        .min(message_count);
    let expected_count = end - first_message_index;
    let mut statement = transaction.prepare(
        "SELECT message_index, value FROM messages
         WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
           AND message_index >= ?4 AND message_index < ?5
         ORDER BY message_index ASC",
    )?;
    let mut rows = statement.query(params![
        logical.pds_generation,
        character_id,
        conversation_id,
        sqlite_i64(first_message_index, "message page first index")?,
        sqlite_i64(end, "message page end index")?,
    ])?;
    let mut messages = Vec::with_capacity(expected_count as usize);
    let mut expected_index = first_message_index;
    while let Some(row) = rows.next()? {
        let message_index = nonnegative_u64(row.get(0)?, "message index")?;
        if message_index != expected_index {
            return validation("conversation messages are not contiguous in affected page");
        }
        messages.push(serde_json::from_str::<Value>(&row.get::<_, String>(1)?)?);
        expected_index += 1;
    }
    if expected_index != end {
        return validation("conversation affected page is incomplete");
    }
    let encoded = encode_message_page(&messages).map_err(codec_error)?;
    transaction.execute(
        "INSERT INTO logical_message_page_sources (
            library_id, generation_id, record_key, page_index,
            first_message_index, message_count, object_hash, object_size
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            logical.library_id,
            logical.generation_id,
            record_key,
            sqlite_i64(page_index, "logical message page index")?,
            sqlite_i64(first_message_index, "logical message first index")?,
            sqlite_i64(expected_count, "logical message page count")?,
            encoded.hash,
            sqlite_i64(encoded.size, "logical message page object size")?,
        ],
    )?;
    Ok(())
}

fn rebuild_conversation_head(
    transaction: &Transaction<'_>,
    logical: &IncrementalLogicalCommit,
    record_key: &str,
    configured_index: i64,
    recent_at: i64,
    detail: Value,
    message_count: u64,
    rebuild_dependencies: bool,
) -> StoreResult<()> {
    let mut statement = transaction.prepare(
        "SELECT page_index, first_message_index, message_count, object_hash, object_size
         FROM logical_message_page_sources
         WHERE library_id = ?1 AND generation_id = ?2 AND record_key = ?3
         ORDER BY page_index ASC",
    )?;
    let mut rows = statement.query(params![
        logical.library_id,
        logical.generation_id,
        record_key,
    ])?;
    let expected_pages = message_count.div_ceil(LOGICAL_MESSAGE_PAGE_SIZE as u64);
    let mut pages = Vec::with_capacity(usize::try_from(expected_pages).map_err(|_| {
        StoreError::Validation {
            message: "logical conversation page count exceeds platform limits".to_owned(),
        }
    })?);
    while let Some(row) = rows.next()? {
        let page_index = nonnegative_u64(row.get(0)?, "logical message page index")?;
        let first_message_index = nonnegative_u64(row.get(1)?, "logical message first index")?;
        let count = nonnegative_u64(row.get(2)?, "logical message page count")?;
        let remaining = message_count
            .checked_sub(first_message_index)
            .ok_or_else(|| StoreError::Validation {
                message: "logical message page starts beyond the conversation".to_owned(),
            })?;
        let expected_first = page_index
            .checked_mul(LOGICAL_MESSAGE_PAGE_SIZE as u64)
            .ok_or_else(|| StoreError::Validation {
                message: "logical message page first index overflow".to_owned(),
            })?;
        if page_index != pages.len() as u64
            || first_message_index != expected_first
            || count != remaining.min(LOGICAL_MESSAGE_PAGE_SIZE as u64)
        {
            return validation("logical conversation page metadata is not contiguous");
        }
        pages.push(LogicalManifestObject {
            hash: row.get(3)?,
            size: nonnegative_u64(row.get(4)?, "logical message page object size")?,
        });
    }
    if pages.len() as u64 != expected_pages {
        return validation("logical conversation page coverage is incomplete");
    }
    let envelope = LogicalRecordEnvelope::Conversation {
        configured_index: nonnegative_u64(configured_index, "conversation configured index")?,
        recent_at,
        detail,
        message_page_hashes: pages.iter().map(|page| page.hash.clone()).collect(),
    };
    let encoded = encode_logical_record(&envelope).map_err(codec_error)?;
    let changed = transaction.execute(
        "UPDATE logical_record_heads
         SET record_kind = 'conversation', state = 'live', object_hash = ?4,
             object_size = ?5, deleted_generation_sequence = NULL
         WHERE library_id = ?1 AND generation_id = ?2 AND record_key = ?3",
        params![
            logical.library_id,
            logical.generation_id,
            record_key,
            encoded.hash,
            sqlite_i64(encoded.size, "logical conversation object size")?,
        ],
    )?;
    if changed != 1 {
        return validation("logical conversation head disappeared during targeted update");
    }
    if !rebuild_dependencies {
        return Ok(());
    }
    let mut unique = BTreeMap::new();
    for page in pages {
        if unique
            .insert(page.hash.clone(), page.size)
            .is_some_and(|old| old != page.size)
        {
            return validation("logical message page hash has conflicting sizes");
        }
    }
    for (hash, size) in unique {
        transaction.execute(
            "INSERT INTO logical_record_dependencies (
                library_id, generation_id, record_key, object_hash, object_size
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                logical.library_id,
                logical.generation_id,
                record_key,
                hash,
                sqlite_i64(size, "logical dependency object size")?,
            ],
        )?;
    }
    Ok(())
}

fn project_asset_alias_by_key(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    logical: &IncrementalLogicalCommit,
    kind: &str,
    logical_key: &str,
) -> StoreResult<()> {
    let row: (
        Option<String>,
        i64,
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        String,
    ) = transaction.query_row(
        "SELECT object_hash, size, mime, name, ext, inlay_type, width, height, metadata
             FROM asset_aliases
             WHERE generation = ?1 AND kind = ?2 AND logical_key = ?3",
        params![logical.pds_generation, kind, logical_key],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
            ))
        },
    )?;
    let size = nonnegative_u64(row.1, "asset alias size")?;
    let metadata = encode_asset_alias_metadata(&LogicalAssetAliasMetadata {
        mime: row.2,
        name: row.3,
        ext: row.4,
        inlay_type: row.5,
        width: row.6,
        height: row.7,
        metadata: serde_json::from_str(&row.8)?,
    })
    .map_err(codec_error)?;
    let dependencies = payload_dependency(cas, row.0.as_deref(), size)?;
    let (locator, record_kind, envelope) = match kind {
        RECORD_KIND_ASSET => (
            LogicalRecordLocator::Asset {
                logical_key: logical_key.to_owned(),
            },
            RECORD_KIND_ASSET,
            LogicalRecordEnvelope::Asset {
                object_hash: row.0,
                size,
                metadata,
            },
        ),
        RECORD_KIND_INLAY => (
            LogicalRecordLocator::Inlay {
                logical_key: logical_key.to_owned(),
            },
            RECORD_KIND_INLAY,
            LogicalRecordEnvelope::Inlay {
                object_hash: row.0,
                size,
                metadata,
            },
        ),
        _ => return validation("asset alias kind is unsupported"),
    };
    insert_live_record(
        transaction,
        &logical.library_id,
        &logical.generation_id,
        encode_logical_record_key(&locator).map_err(codec_error)?,
        record_kind,
        envelope,
        dependencies,
        &[],
    )
}

fn project_cold_alias_by_key(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    logical: &IncrementalLogicalCommit,
    logical_key: &str,
) -> StoreResult<()> {
    let (object_hash, raw_size, metadata): (Option<String>, i64, String) = transaction.query_row(
        "SELECT object_hash, size, metadata FROM cold_aliases
         WHERE generation = ?1 AND key = ?2",
        params![logical.pds_generation, logical_key],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let size = nonnegative_u64(raw_size, "cold alias size")?;
    let dependencies = payload_dependency(cas, object_hash.as_deref(), size)?;
    insert_live_record(
        transaction,
        &logical.library_id,
        &logical.generation_id,
        encode_logical_record_key(&LogicalRecordLocator::Cold {
            logical_key: logical_key.to_owned(),
        })
        .map_err(codec_error)?,
        RECORD_KIND_COLD,
        LogicalRecordEnvelope::Cold {
            object_hash,
            size,
            metadata: serde_json::from_str(&metadata)?,
        },
        dependencies,
        &[],
    )
}

fn validate_pds_projection_contract(connection: &Connection) -> StoreResult<()> {
    let asset_columns = table_columns(connection, "asset_aliases")?;
    for required in [
        "generation",
        "logical_key",
        "object_hash",
        "kind",
        "size",
        "mime",
        "name",
        "ext",
        "inlay_type",
        "width",
        "height",
        "metadata",
    ] {
        if !asset_columns.contains_key(required) {
            return validation(format!(
                "logical projection requires J2 asset_aliases column {required}"
            ));
        }
    }
    let expected_asset_pk = [("generation", 1_i64), ("kind", 2), ("logical_key", 3)];
    if expected_asset_pk
        .iter()
        .any(|(name, position)| asset_columns.get(*name) != Some(position))
    {
        return validation(
            "logical projection requires asset_aliases primary key (generation, kind, logical_key)",
        );
    }

    let cold_columns = table_columns(connection, "cold_aliases")?;
    for required in ["generation", "key", "object_hash", "size", "metadata"] {
        if !cold_columns.contains_key(required) {
            return validation(format!(
                "logical projection requires J2 cold_aliases column {required}"
            ));
        }
    }
    if cold_columns.get("generation") != Some(&1) || cold_columns.get("key") != Some(&2) {
        return validation(
            "logical projection requires cold_aliases primary key (generation, key)",
        );
    }
    Ok(())
}

fn table_columns(connection: &Connection, table: &str) -> StoreResult<BTreeMap<String, i64>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(5)?))
        })?
        .collect::<Result<_, _>>()?;
    Ok(columns)
}

fn project_root(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    library_id: &str,
    generation_id: &str,
    pds_generation: &str,
) -> StoreResult<()> {
    let raw: String = transaction.query_row(
        "SELECT value FROM root WHERE generation = ?1",
        [pds_generation],
        |row| row.get(0),
    )?;
    let mut owner_heads =
        validate_owner_heads(cas, load_owner_heads(transaction, pds_generation, None)?)?;
    let mut value: Value = serde_json::from_str(&raw)?;
    strip_root_owner_properties(&mut value, &mut owner_heads)?;
    let dependencies = owner_dependencies(&owner_heads)?;
    insert_live_record(
        transaction,
        library_id,
        generation_id,
        encode_logical_record_key(&LogicalRecordLocator::Root).map_err(codec_error)?,
        RECORD_KIND_ROOT,
        LogicalRecordEnvelope::Root {
            value,
            owner_heads: logical_owner_heads(&owner_heads),
        },
        dependencies,
        &[],
    )
}

fn project_presets(
    transaction: &Transaction<'_>,
    library_id: &str,
    generation_id: &str,
    pds_generation: &str,
) -> StoreResult<()> {
    let mut statement = transaction.prepare(
        "SELECT preset_id, configured_index, value FROM bot_presets
         WHERE generation = ?1 ORDER BY preset_id ASC",
    )?;
    let mut rows = statement.query([pds_generation])?;
    while let Some(row) = rows.next()? {
        let preset_id: String = row.get(0)?;
        let configured_index = nonnegative_u64(row.get(1)?, "preset configured index")?;
        let raw: String = row.get(2)?;
        insert_live_record(
            transaction,
            library_id,
            generation_id,
            encode_logical_record_key(&LogicalRecordLocator::Preset { preset_id })
                .map_err(codec_error)?,
            RECORD_KIND_PRESET,
            LogicalRecordEnvelope::Preset {
                configured_index,
                value: serde_json::from_str(&raw)?,
            },
            Vec::new(),
            &[],
        )?;
    }
    Ok(())
}

fn project_plugins(
    transaction: &Transaction<'_>,
    library_id: &str,
    generation_id: &str,
    pds_generation: &str,
) -> StoreResult<()> {
    let mut statement = transaction.prepare(
        "SELECT storage_key, ordinal, value FROM plugin_storage
         WHERE generation = ?1 ORDER BY storage_key ASC",
    )?;
    let mut rows = statement.query([pds_generation])?;
    while let Some(row) = rows.next()? {
        let storage_key: String = row.get(0)?;
        let ordinal = nonnegative_u64(row.get(1)?, "plugin storage ordinal")?;
        let raw: String = row.get(2)?;
        insert_live_record(
            transaction,
            library_id,
            generation_id,
            encode_logical_record_key(&LogicalRecordLocator::Plugin { storage_key })
                .map_err(codec_error)?,
            RECORD_KIND_PLUGIN,
            LogicalRecordEnvelope::Plugin {
                ordinal,
                value: serde_json::from_str(&raw)?,
            },
            Vec::new(),
            &[],
        )?;
    }
    Ok(())
}

fn project_characters(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    library_id: &str,
    generation_id: &str,
    pds_generation: &str,
) -> StoreResult<()> {
    let mut statement = transaction.prepare(
        "SELECT character_id, configured_index, detail FROM characters
         WHERE generation = ?1 ORDER BY character_id ASC",
    )?;
    let mut rows = statement.query([pds_generation])?;
    while let Some(row) = rows.next()? {
        let character_id: String = row.get(0)?;
        let configured_index = nonnegative_u64(row.get(1)?, "character configured index")?;
        let raw: String = row.get(2)?;
        let mut owner_heads = validate_owner_heads(
            cas,
            load_owner_heads(transaction, pds_generation, Some(&character_id))?,
        )?;
        let mut detail: Value = serde_json::from_str(&raw)?;
        strip_character_owner_property(&mut detail, &character_id, &mut owner_heads)?;
        let dependencies = owner_dependencies(&owner_heads)?;
        insert_live_record(
            transaction,
            library_id,
            generation_id,
            encode_logical_record_key(&LogicalRecordLocator::Character { character_id })
                .map_err(codec_error)?,
            RECORD_KIND_CHARACTER,
            LogicalRecordEnvelope::Character {
                configured_index,
                detail,
                owner_heads: logical_owner_heads(&owner_heads),
            },
            dependencies,
            &[],
        )?;
    }
    Ok(())
}

fn project_conversations(
    transaction: &Transaction<'_>,
    library_id: &str,
    generation_id: &str,
    pds_generation: &str,
) -> StoreResult<()> {
    let mut statement = transaction.prepare(
        "SELECT character_id, conversation_id, configured_index, recent_at, detail, message_count
         FROM conversations WHERE generation = ?1
         ORDER BY character_id ASC, conversation_id ASC",
    )?;
    let mut rows = statement.query([pds_generation])?;
    while let Some(row) = rows.next()? {
        let character_id: String = row.get(0)?;
        let conversation_id: String = row.get(1)?;
        let configured_index = nonnegative_u64(row.get(2)?, "conversation configured index")?;
        let recent_at: i64 = row.get(3)?;
        let raw: String = row.get(4)?;
        let expected_count = nonnegative_u64(row.get(5)?, "conversation message count")?;
        let key = encode_logical_record_key(&LogicalRecordLocator::Conversation {
            character_id: character_id.clone(),
            conversation_id: conversation_id.clone(),
        })
        .map_err(codec_error)?;
        let pages = project_message_pages(
            transaction,
            pds_generation,
            &character_id,
            &conversation_id,
            expected_count,
        )?;
        let dependencies = pages.iter().map(|page| page.object.clone()).collect();
        let message_page_hashes = pages.iter().map(|page| page.object.hash.clone()).collect();
        insert_live_record(
            transaction,
            library_id,
            generation_id,
            key,
            RECORD_KIND_CONVERSATION,
            LogicalRecordEnvelope::Conversation {
                configured_index,
                recent_at,
                detail: serde_json::from_str(&raw)?,
                message_page_hashes,
            },
            dependencies,
            &pages,
        )?;
    }
    Ok(())
}

fn project_message_pages(
    transaction: &Transaction<'_>,
    pds_generation: &str,
    character_id: &str,
    conversation_id: &str,
    expected_count: u64,
) -> StoreResult<Vec<PageSource>> {
    let (stored_count, first_index, last_index): (i64, Option<i64>, Option<i64>) = transaction
        .query_row(
            "SELECT COUNT(*), MIN(message_index), MAX(message_index) FROM messages
             WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
            params![pds_generation, character_id, conversation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    let stored_count = nonnegative_u64(stored_count, "stored message count")?;
    let first_index = first_index
        .map(|value| nonnegative_u64(value, "first message index"))
        .transpose()?;
    let last_index = last_index
        .map(|value| nonnegative_u64(value, "last message index"))
        .transpose()?;
    let expected_first = (expected_count > 0).then_some(0);
    let expected_last = expected_count.checked_sub(1);
    if stored_count != expected_count
        || first_index != expected_first
        || last_index != expected_last
    {
        return validation("conversation message count or index range is inconsistent");
    }
    let mut pages = Vec::new();
    let mut next_index = 0_u64;
    while next_index < expected_count {
        let mut statement = transaction.prepare(
            "SELECT message_index, value FROM messages
             WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
               AND message_index >= ?4
             ORDER BY message_index ASC LIMIT ?5",
        )?;
        let mut rows = statement.query(params![
            pds_generation,
            character_id,
            conversation_id,
            sqlite_i64(next_index, "message page start index")?,
            LOGICAL_MESSAGE_PAGE_SIZE as i64,
        ])?;
        let first_message_index = next_index;
        let mut messages = Vec::with_capacity(LOGICAL_MESSAGE_PAGE_SIZE);
        while let Some(row) = rows.next()? {
            let message_index = nonnegative_u64(row.get(0)?, "message index")?;
            if message_index != next_index {
                return validation("conversation messages are not contiguous");
            }
            let raw: String = row.get(1)?;
            messages.push(serde_json::from_str(&raw)?);
            next_index += 1;
        }
        if messages.is_empty() {
            return validation("conversation message count exceeds stored messages");
        }
        let encoded = encode_message_page(&messages).map_err(codec_error)?;
        pages.push(PageSource {
            page_index: u64::try_from(pages.len()).map_err(|_| StoreError::Validation {
                message: "conversation page index overflow".to_owned(),
            })?,
            first_message_index,
            message_count: u64::try_from(messages.len()).map_err(|_| StoreError::Validation {
                message: "conversation page message count overflow".to_owned(),
            })?,
            object: manifest_object(&encoded),
        });
    }
    Ok(pages)
}

fn project_asset_aliases(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    library_id: &str,
    generation_id: &str,
    pds_generation: &str,
) -> StoreResult<()> {
    let mut statement = transaction.prepare(
        "SELECT kind, logical_key, object_hash, size, mime, name, ext,
                inlay_type, width, height, metadata
         FROM asset_aliases
         WHERE generation = ?1 ORDER BY kind ASC, logical_key ASC",
    )?;
    let mut rows = statement.query([pds_generation])?;
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let logical_key: String = row.get(1)?;
        let object_hash: Option<String> = row.get(2)?;
        let size = nonnegative_u64(row.get(3)?, "asset alias size")?;
        let metadata = encode_asset_alias_metadata(&LogicalAssetAliasMetadata {
            mime: row.get(4)?,
            name: row.get(5)?,
            ext: row.get(6)?,
            inlay_type: row.get(7)?,
            width: row.get(8)?,
            height: row.get(9)?,
            metadata: serde_json::from_str::<Value>(&row.get::<_, String>(10)?)?,
        })
        .map_err(codec_error)?;
        let dependencies = payload_dependency(cas, object_hash.as_deref(), size)?;
        let (locator, record_kind, envelope) = match kind.as_str() {
            RECORD_KIND_ASSET => (
                LogicalRecordLocator::Asset { logical_key },
                RECORD_KIND_ASSET,
                LogicalRecordEnvelope::Asset {
                    object_hash,
                    size,
                    metadata,
                },
            ),
            RECORD_KIND_INLAY => (
                LogicalRecordLocator::Inlay { logical_key },
                RECORD_KIND_INLAY,
                LogicalRecordEnvelope::Inlay {
                    object_hash,
                    size,
                    metadata,
                },
            ),
            _ => return validation("asset alias kind is unsupported"),
        };
        insert_live_record(
            transaction,
            library_id,
            generation_id,
            encode_logical_record_key(&locator).map_err(codec_error)?,
            record_kind,
            envelope,
            dependencies,
            &[],
        )?;
    }
    Ok(())
}

fn project_cold_aliases(
    transaction: &Transaction<'_>,
    cas: &PayloadCas,
    library_id: &str,
    generation_id: &str,
    pds_generation: &str,
) -> StoreResult<()> {
    let mut statement = transaction.prepare(
        "SELECT key, object_hash, size, metadata FROM cold_aliases
         WHERE generation = ?1 ORDER BY key ASC",
    )?;
    let mut rows = statement.query([pds_generation])?;
    while let Some(row) = rows.next()? {
        let logical_key: String = row.get(0)?;
        let object_hash: Option<String> = row.get(1)?;
        let size = nonnegative_u64(row.get(2)?, "cold alias size")?;
        let metadata: String = row.get(3)?;
        let dependencies = payload_dependency(cas, object_hash.as_deref(), size)?;
        insert_live_record(
            transaction,
            library_id,
            generation_id,
            encode_logical_record_key(&LogicalRecordLocator::Cold { logical_key })
                .map_err(codec_error)?,
            RECORD_KIND_COLD,
            LogicalRecordEnvelope::Cold {
                object_hash,
                size,
                metadata: serde_json::from_str(&metadata)?,
            },
            dependencies,
            &[],
        )?;
    }
    Ok(())
}

fn load_owner_heads(
    connection: &Connection,
    pds_generation: &str,
    character_id: Option<&str>,
) -> StoreResult<Vec<LogicalOwnerHead>> {
    let (sql, locator): (&str, Option<&str>) = match character_id {
        Some(character_id) => (
            "SELECT owner_kind, owner_locator, present, manifest_hash, entry_count
             FROM asset_owner_heads
             WHERE generation = ?1 AND owner_kind = 'character-additional-assets'
               AND owner_locator = ?2
             ORDER BY owner_kind ASC, owner_locator ASC",
            Some(character_id),
        ),
        None => (
            "SELECT owner_kind, owner_locator, present, manifest_hash, entry_count
             FROM asset_owner_heads
             WHERE generation = ?1 AND owner_kind IN (
                'root-module-assets', 'persona-embedded-module-assets'
             )
             ORDER BY owner_kind ASC, CAST(owner_locator AS INTEGER) ASC",
            None,
        ),
    };
    let mut statement = connection.prepare(sql)?;
    let mut rows = match locator {
        Some(locator) => statement.query(params![pds_generation, locator])?,
        None => statement.query([pds_generation])?,
    };
    let mut heads = Vec::new();
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let locator: String = row.get(1)?;
        let present: bool = row.get(2)?;
        let manifest_hash: Option<String> = row.get(3)?;
        let entry_count = nonnegative_u64(row.get(4)?, "owner entry count")?;
        let owner = match kind.as_str() {
            "character-additional-assets" => LogicalOwnerLocator::CharacterAdditional {
                character_id: locator,
            },
            "root-module-assets" => LogicalOwnerLocator::RootModule {
                index: parse_owner_index(&locator)?,
            },
            "persona-embedded-module-assets" => LogicalOwnerLocator::PersonaEmbeddedModule {
                index: parse_owner_index(&locator)?,
            },
            _ => return validation("asset owner kind is unsupported"),
        };
        let head = if present {
            let hash = manifest_hash.ok_or_else(|| StoreError::Validation {
                message: "present owner head has no manifest hash".to_owned(),
            })?;
            LogicalOwnerHead::unpositioned_present(owner, hash, entry_count).map_err(codec_error)?
        } else {
            if manifest_hash.is_some() || entry_count != 0 {
                return validation("absent owner head contains manifest data");
            }
            LogicalOwnerHead::absent(owner)
        };
        heads.push(head);
    }
    Ok(heads)
}

fn validate_owner_heads(
    cas: &PayloadCas,
    heads: Vec<LogicalOwnerHead>,
) -> StoreResult<Vec<ValidatedOwnerHead>> {
    let mut validated = Vec::with_capacity(heads.len());
    for head in heads {
        let Some(manifest_hash) = head.manifest_hash.as_deref() else {
            validated.push(ValidatedOwnerHead {
                head,
                tuples: None,
                dependencies: Vec::new(),
            });
            continue;
        };
        let bytes = cas
            .read_object(manifest_hash)?
            .ok_or_else(|| StoreError::Validation {
                message: format!("referenced owner manifest {manifest_hash} is missing"),
            })?;
        verify_object_bytes(&bytes, manifest_hash, bytes.len() as u64)?;
        let entries = decode_owner_manifest(&bytes).map_err(codec_error)?;
        if encode_owner_manifest(&entries).map_err(codec_error)? != bytes {
            return validation("owner manifest bytes are not canonical");
        }
        if entries.len() as u64 != head.entry_count {
            return validation("owner manifest entry count does not match its head");
        }
        let mut dependencies = BTreeMap::from([(manifest_hash.to_owned(), bytes.len() as u64)]);
        let mut tuples = Vec::with_capacity(entries.len());
        for entry in entries {
            tuples.push(Value::Array(
                entry.tuple.into_iter().map(Value::String).collect(),
            ));
            if let Some(payload_hash) = entry.payload_hash {
                let hash = hex::encode(payload_hash);
                let size = require_cas_size(cas, &hash)?;
                if dependencies
                    .insert(hash, size)
                    .is_some_and(|old| old != size)
                {
                    return validation("owner dependency hash has conflicting sizes");
                }
            }
        }
        validated.push(ValidatedOwnerHead {
            head,
            tuples: Some(tuples),
            dependencies: dependencies
                .into_iter()
                .map(|(hash, size)| LogicalManifestObject { hash, size })
                .collect(),
        });
    }
    Ok(validated)
}

fn logical_owner_heads(heads: &[ValidatedOwnerHead]) -> Vec<LogicalOwnerHead> {
    heads.iter().map(|head| head.head.clone()).collect()
}

fn owner_dependencies(heads: &[ValidatedOwnerHead]) -> StoreResult<Vec<LogicalManifestObject>> {
    let mut dependencies = BTreeMap::new();
    for dependency in heads.iter().flat_map(|head| &head.dependencies) {
        if dependencies
            .insert(dependency.hash.clone(), dependency.size)
            .is_some_and(|old| old != dependency.size)
        {
            return validation("owner dependency hash has conflicting sizes");
        }
    }
    Ok(dependencies
        .into_iter()
        .map(|(hash, size)| LogicalManifestObject { hash, size })
        .collect())
}

fn strip_owner_property(
    parent: &mut serde_json::Map<String, Value>,
    property: &str,
    head: &mut ValidatedOwnerHead,
) -> StoreResult<()> {
    let property_index = parent.keys().position(|key| key == property);
    if property_index.is_some() != head.head.present {
        return validation("owner head property presence does not match its parent record");
    }
    if let Some(expected) = &head.tuples {
        if parent.get(property) != Some(&Value::Array(expected.clone())) {
            return validation("owner manifest tuples do not match their parent record");
        }
        head.head.property_index = Some(
            u64::try_from(property_index.expect("present owner property has an index")).map_err(
                |_| StoreError::Validation {
                    message: "owner property index exceeds the wire range".to_owned(),
                },
            )?,
        );
        parent.shift_remove(property);
    } else {
        head.head.property_index = None;
    }
    Ok(())
}

fn strip_root_owner_properties(
    value: &mut Value,
    heads: &mut [ValidatedOwnerHead],
) -> StoreResult<()> {
    let root = value
        .as_object_mut()
        .ok_or_else(|| StoreError::Validation {
            message: "logical root projection requires an object".to_owned(),
        })?;
    let mut by_identity = BTreeMap::new();
    for (head_index, head) in heads.iter().enumerate() {
        let identity = match &head.head.owner {
            LogicalOwnerLocator::RootModule { index } => format!("module:{index}"),
            LogicalOwnerLocator::PersonaEmbeddedModule { index } => format!("persona:{index}"),
            LogicalOwnerLocator::CharacterAdditional { .. } => {
                return validation("root logical record contains a character owner head")
            }
        };
        if by_identity.insert(identity, head_index).is_some() {
            return validation("root logical record contains duplicate owner heads");
        }
    }
    let mut expected = 0_usize;
    if let Some(modules) = root.get_mut("modules") {
        let modules = modules
            .as_array_mut()
            .ok_or_else(|| StoreError::Validation {
                message: "root modules must be an array".to_owned(),
            })?;
        for (index, module) in modules.iter_mut().enumerate() {
            let module = module
                .as_object_mut()
                .ok_or_else(|| StoreError::Validation {
                    message: "root module must be an object".to_owned(),
                })?;
            let head_index = *by_identity.get(&format!("module:{index}")).ok_or_else(|| {
                StoreError::Validation {
                    message: "root module owner head coverage is incomplete".to_owned(),
                }
            })?;
            strip_owner_property(module, "assets", &mut heads[head_index])?;
            expected += 1;
        }
    }
    if let Some(personas) = root.get_mut("personas") {
        let personas = personas
            .as_array_mut()
            .ok_or_else(|| StoreError::Validation {
                message: "root personas must be an array".to_owned(),
            })?;
        for (index, persona) in personas.iter_mut().enumerate() {
            let persona = persona
                .as_object_mut()
                .ok_or_else(|| StoreError::Validation {
                    message: "root persona must be an object".to_owned(),
                })?;
            let Some(embedded) = persona.get_mut("embeddedModule") else {
                continue;
            };
            let embedded = embedded
                .as_object_mut()
                .ok_or_else(|| StoreError::Validation {
                    message: "persona embeddedModule must be an object".to_owned(),
                })?;
            let head_index = *by_identity
                .get(&format!("persona:{index}"))
                .ok_or_else(|| StoreError::Validation {
                    message: "persona embedded module owner head coverage is incomplete".to_owned(),
                })?;
            strip_owner_property(embedded, "assets", &mut heads[head_index])?;
            expected += 1;
        }
    }
    if by_identity.len() != expected {
        return validation("root logical record contains owner heads for missing occurrences");
    }
    Ok(())
}

fn strip_character_owner_property(
    detail: &mut Value,
    character_id: &str,
    heads: &mut [ValidatedOwnerHead],
) -> StoreResult<()> {
    let [head] = heads else {
        return validation("character owner head coverage must contain exactly one head");
    };
    if !matches!(
        &head.head.owner,
        LogicalOwnerLocator::CharacterAdditional { character_id: owner_id }
            if owner_id == character_id
    ) {
        return validation("character logical record owner head does not match its key");
    }
    let detail = detail
        .as_object_mut()
        .ok_or_else(|| StoreError::Validation {
            message: "character logical projection requires an object".to_owned(),
        })?;
    strip_owner_property(detail, "additionalAssets", head)
}

fn payload_dependency(
    cas: &PayloadCas,
    object_hash: Option<&str>,
    declared_size: u64,
) -> StoreResult<Vec<LogicalManifestObject>> {
    let Some(hash) = object_hash else {
        return Ok(Vec::new());
    };
    let actual_size = require_cas_size(cas, hash)?;
    if actual_size != declared_size {
        return validation(format!(
            "payload alias size {declared_size} does not match CAS size {actual_size}"
        ));
    }
    Ok(vec![LogicalManifestObject {
        hash: hash.to_owned(),
        size: actual_size,
    }])
}

fn require_cas_size(cas: &PayloadCas, hash: &str) -> StoreResult<u64> {
    cas.stat_object(hash)?
        .ok_or_else(|| StoreError::Validation {
            message: format!("referenced CAS object {hash} is missing"),
        })
}

#[allow(clippy::too_many_arguments)]
fn insert_live_record(
    transaction: &Transaction<'_>,
    library_id: &str,
    generation_id: &str,
    record_key: String,
    record_kind: &str,
    envelope: LogicalRecordEnvelope,
    dependencies: Vec<LogicalManifestObject>,
    pages: &[PageSource],
) -> StoreResult<()> {
    let encoded = encode_logical_record(&envelope).map_err(codec_error)?;
    transaction.execute(
        "INSERT INTO logical_record_heads (
            library_id, generation_id, record_key, record_kind, state,
            object_hash, object_size, deleted_generation_sequence
         ) VALUES (?1, ?2, ?3, ?4, 'live', ?5, ?6, NULL)",
        params![
            library_id,
            generation_id,
            record_key,
            record_kind,
            encoded.hash,
            sqlite_i64(encoded.size, "logical record object size")?,
        ],
    )?;
    let mut unique = BTreeMap::new();
    for dependency in dependencies {
        if let Some(existing) = unique.insert(dependency.hash.clone(), dependency.size) {
            if existing != dependency.size {
                return validation("logical dependency hash has conflicting sizes");
            }
        }
    }
    for (hash, size) in unique {
        transaction.execute(
            "INSERT INTO logical_record_dependencies (
                library_id, generation_id, record_key, object_hash, object_size
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                library_id,
                generation_id,
                record_key,
                hash,
                sqlite_i64(size, "logical dependency object size")?
            ],
        )?;
    }
    for page in pages {
        transaction.execute(
            "INSERT INTO logical_message_page_sources (
                library_id, generation_id, record_key, page_index,
                first_message_index, message_count, object_hash, object_size
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                library_id,
                generation_id,
                record_key,
                sqlite_i64(page.page_index, "logical message page index")?,
                sqlite_i64(page.first_message_index, "logical message first index")?,
                sqlite_i64(page.message_count, "logical message page count")?,
                page.object.hash,
                sqlite_i64(page.object.size, "logical message page object size")?,
            ],
        )?;
    }
    Ok(())
}

fn project_tombstones(
    transaction: &Transaction<'_>,
    library_id: &str,
    generation_id: &str,
    parent_generation_id: &str,
    deleted_generation_sequence: &str,
) -> StoreResult<()> {
    transaction.execute(
        "INSERT INTO logical_record_heads (
            library_id, generation_id, record_key, record_kind, state,
            object_hash, object_size, deleted_generation_sequence
         )
         SELECT parent.library_id, ?2, parent.record_key, parent.record_kind, 'tombstone',
                NULL, 0,
                CASE WHEN parent.state = 'tombstone'
                     THEN parent.deleted_generation_sequence ELSE ?4 END
         FROM logical_record_heads AS parent
         WHERE parent.library_id = ?1 AND parent.generation_id = ?3
           AND NOT EXISTS (
               SELECT 1 FROM logical_record_heads AS current
               WHERE current.library_id = ?1 AND current.generation_id = ?2
                 AND current.record_key = parent.record_key
           )",
        params![
            library_id,
            generation_id,
            parent_generation_id,
            deleted_generation_sequence,
        ],
    )?;
    Ok(())
}

pub(super) fn scan_compact_manifest(
    connection: &Connection,
    library_id: &str,
    generation_id: &str,
    require_complete: bool,
) -> StoreResult<BuiltIndexedLogicalManifest> {
    let metadata = logical_generation_metadata(connection, library_id, generation_id)?;
    if require_complete && metadata.state != "complete" {
        return validation("logical generation index is not complete");
    }
    let mut dependencies_by_record = BTreeMap::<String, Vec<LogicalManifestObject>>::new();
    let mut dependency_statement = connection.prepare(
        "SELECT record_key, object_hash, object_size
         FROM logical_record_dependencies
         WHERE library_id = ?1 AND generation_id = ?2
         ORDER BY record_key ASC, object_hash ASC",
    )?;
    let mut dependency_rows = dependency_statement.query(params![library_id, generation_id])?;
    while let Some(row) = dependency_rows.next()? {
        dependencies_by_record
            .entry(row.get(0)?)
            .or_default()
            .push(LogicalManifestObject {
                hash: row.get(1)?,
                size: nonnegative_u64(row.get(2)?, "indexed logical dependency size")?,
            });
    }
    drop(dependency_rows);
    drop(dependency_statement);

    let mut statement = connection.prepare(
        "SELECT record_key, state, object_hash, object_size, deleted_generation_sequence
         FROM logical_record_heads
         WHERE library_id = ?1 AND generation_id = ?2 ORDER BY record_key ASC",
    )?;
    let mut rows = statement.query(params![library_id, generation_id])?;
    let mut records = Vec::new();
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let state: String = row.get(1)?;
        if state == "live" {
            let hash: String = row.get(2)?;
            let size = nonnegative_u64(row.get(3)?, "indexed logical object size")?;
            let dependencies = dependencies_by_record.remove(&key).unwrap_or_default();
            records.push(IndexedLogicalRecord::live(
                key,
                LogicalManifestObject { hash, size },
                dependencies,
            ));
        } else if state == "tombstone" {
            records.push(IndexedLogicalRecord::tombstone(key, row.get(4)?));
        } else {
            return validation("logical record head state is invalid");
        }
    }
    if !dependencies_by_record.is_empty() {
        return validation("logical dependencies reference missing record heads");
    }
    let built = build_indexed_logical_manifest(IndexedLogicalManifestBuilderInput {
        library_id: metadata.library_id,
        generation: metadata.generation_id,
        generation_sequence: metadata.generation_sequence,
        parent_generation: metadata.parent_generation_id,
        source_revision: nonnegative_u64(metadata.source_revision, "logical source revision")?,
        records,
    })
    .map_err(codec_error)?;
    if require_complete && metadata.manifest_hash.as_deref() != Some(&built.manifest_hash) {
        return validation("complete logical index manifest hash does not match compact metadata");
    }
    Ok(built)
}

fn logical_generation_metadata(
    connection: &Connection,
    library_id: &str,
    generation_id: &str,
) -> StoreResult<LogicalGenerationMetadata> {
    connection
        .query_row(
            "SELECT library_id, generation_id, generation_sequence, parent_generation_id,
                    source_revision, state, manifest_hash
             FROM logical_sync_generations
             WHERE library_id = ?1 AND generation_id = ?2",
            params![library_id, generation_id],
            |row| {
                Ok(LogicalGenerationMetadata {
                    library_id: row.get(0)?,
                    generation_id: row.get(1)?,
                    generation_sequence: row.get(2)?,
                    parent_generation_id: row.get(3)?,
                    source_revision: row.get(4)?,
                    state: row.get(5)?,
                    manifest_hash: row.get(6)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| StoreError::Validation {
            message: "logical generation index does not exist".to_owned(),
        })
}

fn require_complete_generation(
    connection: &Connection,
    library_id: &str,
    generation_id: &str,
) -> StoreResult<String> {
    connection
        .query_row(
            "SELECT pds_generation FROM logical_sync_generations
             WHERE library_id = ?1 AND generation_id = ?2 AND state = 'complete'
               AND manifest_hash IS NOT NULL",
            params![library_id, generation_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Validation {
            message: "logical generation index is absent or incomplete".to_owned(),
        })
}

fn referenced_object_size(
    connection: &Connection,
    library_id: &str,
    generation_id: &str,
    object_hash: &str,
) -> StoreResult<u64> {
    let mut statement = connection.prepare(
        "SELECT DISTINCT object_size FROM (
            SELECT object_size FROM logical_record_heads
            WHERE library_id = ?1 AND generation_id = ?2 AND object_hash = ?3
            UNION ALL
            SELECT object_size FROM logical_record_dependencies
            WHERE library_id = ?1 AND generation_id = ?2 AND object_hash = ?3
         ) ORDER BY object_size ASC",
    )?;
    let sizes = statement
        .query_map(params![library_id, generation_id, object_hash], |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<Result<Vec<i64>, _>>()?
        .into_iter()
        .map(|size| nonnegative_u64(size, "indexed object size"))
        .collect::<StoreResult<Vec<_>>>()?;
    match sizes.as_slice() {
        [size] => Ok(*size),
        [] => validation("requested object is not referenced by the logical generation"),
        _ => validation("requested object hash has conflicting indexed sizes"),
    }
}

fn reconstruct_record(
    connection: &Connection,
    cas: &PayloadCas,
    library_id: &str,
    generation_id: &str,
    pds_generation: &str,
    record_key: &str,
) -> StoreResult<Vec<u8>> {
    let locator = decode_logical_record_key(record_key).map_err(codec_error)?;
    let envelope = match locator {
        LogicalRecordLocator::Root => {
            let raw = required_text(
                connection,
                "SELECT value FROM root WHERE generation = ?1",
                params![pds_generation],
                "root record source is missing",
            )?;
            let mut owner_heads =
                validate_owner_heads(cas, load_owner_heads(connection, pds_generation, None)?)?;
            let mut value: Value = serde_json::from_str(&raw)?;
            strip_root_owner_properties(&mut value, &mut owner_heads)?;
            LogicalRecordEnvelope::Root {
                value,
                owner_heads: logical_owner_heads(&owner_heads),
            }
        }
        LogicalRecordLocator::Preset { preset_id } => {
            let (configured_index, raw): (i64, String) = connection
                .query_row(
                    "SELECT configured_index, value FROM bot_presets
                     WHERE generation = ?1 AND preset_id = ?2",
                    params![pds_generation, preset_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| missing_source("preset"))?;
            LogicalRecordEnvelope::Preset {
                configured_index: nonnegative_u64(configured_index, "preset configured index")?,
                value: serde_json::from_str(&raw)?,
            }
        }
        LogicalRecordLocator::Plugin { storage_key } => {
            let (ordinal, raw): (i64, String) = connection
                .query_row(
                    "SELECT ordinal, value FROM plugin_storage
                     WHERE generation = ?1 AND storage_key = ?2",
                    params![pds_generation, storage_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| missing_source("plugin"))?;
            LogicalRecordEnvelope::Plugin {
                ordinal: nonnegative_u64(ordinal, "plugin storage ordinal")?,
                value: serde_json::from_str(&raw)?,
            }
        }
        LogicalRecordLocator::Character { character_id } => {
            let (configured_index, raw): (i64, String) = connection
                .query_row(
                    "SELECT configured_index, detail FROM characters
                     WHERE generation = ?1 AND character_id = ?2",
                    params![pds_generation, character_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| missing_source("character"))?;
            let mut owner_heads = validate_owner_heads(
                cas,
                load_owner_heads(connection, pds_generation, Some(&character_id))?,
            )?;
            let mut detail: Value = serde_json::from_str(&raw)?;
            strip_character_owner_property(&mut detail, &character_id, &mut owner_heads)?;
            LogicalRecordEnvelope::Character {
                configured_index: nonnegative_u64(configured_index, "character configured index")?,
                detail,
                owner_heads: logical_owner_heads(&owner_heads),
            }
        }
        LogicalRecordLocator::Conversation {
            character_id,
            conversation_id,
        } => {
            let (configured_index, recent_at, raw): (i64, i64, String) = connection
                .query_row(
                    "SELECT configured_index, recent_at, detail FROM conversations
                     WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3",
                    params![pds_generation, character_id, conversation_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| missing_source("conversation"))?;
            let mut statement = connection.prepare(
                "SELECT object_hash FROM logical_message_page_sources
                 WHERE library_id = ?1 AND generation_id = ?2 AND record_key = ?3
                 ORDER BY page_index ASC",
            )?;
            let message_page_hashes = statement
                .query_map(params![library_id, generation_id, record_key], |row| {
                    row.get(0)
                })?
                .collect::<Result<Vec<String>, _>>()?;
            LogicalRecordEnvelope::Conversation {
                configured_index: nonnegative_u64(
                    configured_index,
                    "conversation configured index",
                )?,
                recent_at,
                detail: serde_json::from_str(&raw)?,
                message_page_hashes,
            }
        }
        LogicalRecordLocator::Asset { logical_key } => {
            reconstruct_asset_alias(connection, pds_generation, RECORD_KIND_ASSET, &logical_key)?
        }
        LogicalRecordLocator::Inlay { logical_key } => {
            reconstruct_asset_alias(connection, pds_generation, RECORD_KIND_INLAY, &logical_key)?
        }
        LogicalRecordLocator::Cold { logical_key } => {
            let (object_hash, size, metadata): (Option<String>, i64, String) = connection
                .query_row(
                    "SELECT object_hash, size, metadata FROM cold_aliases
                     WHERE generation = ?1 AND key = ?2",
                    params![pds_generation, logical_key],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?
                .ok_or_else(|| missing_source("cold"))?;
            LogicalRecordEnvelope::Cold {
                object_hash,
                size: nonnegative_u64(size, "cold alias size")?,
                metadata: serde_json::from_str(&metadata)?,
            }
        }
    };
    Ok(encode_logical_record(&envelope).map_err(codec_error)?.bytes)
}

fn reconstruct_asset_alias(
    connection: &Connection,
    pds_generation: &str,
    kind: &str,
    logical_key: &str,
) -> StoreResult<LogicalRecordEnvelope> {
    let (object_hash, size, mime, name, ext, inlay_type, width, height, metadata): (
        Option<String>,
        i64,
        String,
        String,
        String,
        Option<String>,
        Option<i64>,
        Option<i64>,
        String,
    ) = connection
        .query_row(
            "SELECT object_hash, size, mime, name, ext, inlay_type, width, height, metadata
             FROM asset_aliases
             WHERE generation = ?1 AND kind = ?2 AND logical_key = ?3",
            params![pds_generation, kind, logical_key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| missing_source(kind))?;
    let size = nonnegative_u64(size, "asset alias size")?;
    let metadata = encode_asset_alias_metadata(&LogicalAssetAliasMetadata {
        mime,
        name,
        ext,
        inlay_type,
        width,
        height,
        metadata: serde_json::from_str(&metadata)?,
    })
    .map_err(codec_error)?;
    Ok(if kind == RECORD_KIND_ASSET {
        LogicalRecordEnvelope::Asset {
            object_hash,
            size,
            metadata,
        }
    } else {
        LogicalRecordEnvelope::Inlay {
            object_hash,
            size,
            metadata,
        }
    })
}

fn reconstruct_page(
    connection: &Connection,
    pds_generation: &str,
    record_key: &str,
    first_message_index: u64,
    message_count: u64,
) -> StoreResult<Vec<u8>> {
    let LogicalRecordLocator::Conversation {
        character_id,
        conversation_id,
    } = decode_logical_record_key(record_key).map_err(codec_error)?
    else {
        return validation("message page source does not reference a conversation");
    };
    let end = first_message_index
        .checked_add(message_count)
        .ok_or_else(|| StoreError::Validation {
            message: "message page range overflow".to_owned(),
        })?;
    let mut statement = connection.prepare(
        "SELECT message_index, value FROM messages
         WHERE generation = ?1 AND character_id = ?2 AND conversation_id = ?3
           AND message_index >= ?4 AND message_index < ?5
         ORDER BY message_index ASC",
    )?;
    let mut rows = statement.query(params![
        pds_generation,
        character_id,
        conversation_id,
        sqlite_i64(first_message_index, "message page first index")?,
        sqlite_i64(end, "message page end index")?,
    ])?;
    let mut messages = Vec::with_capacity(message_count as usize);
    let mut expected = first_message_index;
    while let Some(row) = rows.next()? {
        let index = nonnegative_u64(row.get(0)?, "message index")?;
        if index != expected {
            return validation("message page source is no longer contiguous");
        }
        let raw: String = row.get(1)?;
        messages.push(serde_json::from_str(&raw)?);
        expected += 1;
    }
    if expected != end {
        return validation("message page source is incomplete");
    }
    Ok(encode_message_page(&messages).map_err(codec_error)?.bytes)
}

fn required_text(
    connection: &Connection,
    sql: &str,
    parameters: impl rusqlite::Params,
    message: &str,
) -> StoreResult<String> {
    connection
        .query_row(sql, parameters, |row| row.get(0))
        .optional()?
        .ok_or_else(|| StoreError::Validation {
            message: message.to_owned(),
        })
}

fn manifest_object(encoded: &EncodedLogicalObject) -> LogicalManifestObject {
    LogicalManifestObject {
        hash: encoded.hash.clone(),
        size: encoded.size,
    }
}

fn parse_owner_index(value: &str) -> StoreResult<u64> {
    let parsed = value.parse::<u64>().map_err(|_| StoreError::Validation {
        message: "asset owner locator is not a nonnegative integer".to_owned(),
    })?;
    if parsed.to_string() != value {
        return validation("asset owner locator is not canonical");
    }
    Ok(parsed)
}

fn verify_object_bytes(bytes: &[u8], hash: &str, size: u64) -> StoreResult<()> {
    if bytes.len() as u64 != size || hex::encode(Sha256::digest(bytes)) != hash {
        return validation("reconstructed logical object failed hash or size verification");
    }
    Ok(())
}

fn nonnegative_u64(value: i64, description: &str) -> StoreResult<u64> {
    u64::try_from(value).map_err(|_| StoreError::Validation {
        message: format!("{description} must be nonnegative"),
    })
}

fn sqlite_i64(value: u64, description: &str) -> StoreResult<i64> {
    i64::try_from(value).map_err(|_| StoreError::Validation {
        message: format!("{description} exceeds the SQLite integer range"),
    })
}

fn unix_millis() -> StoreResult<i64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::Store {
            message: "system clock is before the Unix epoch".to_owned(),
        })?
        .as_millis();
    i64::try_from(millis).map_err(|_| StoreError::Store {
        message: "system clock exceeds SQLite timestamp range".to_owned(),
    })
}

fn schema_error(error: logical_schema::LogicalSchemaError) -> StoreError {
    StoreError::Validation {
        message: error.to_string(),
    }
}

fn codec_error(error: impl std::fmt::Display) -> StoreError {
    StoreError::Validation {
        message: error.to_string(),
    }
}

fn missing_source(kind: &str) -> StoreError {
    StoreError::Validation {
        message: format!("{kind} logical record source is missing"),
    }
}

fn validation<T>(message: impl Into<String>) -> StoreResult<T> {
    Err(StoreError::Validation {
        message: message.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_repository::owner_manifest_codec::{
        encode_owner_manifest, OwnerManifestEntry,
    };
    use crate::peer_sync::logical_delta::{
        decode_asset_alias_metadata, decode_logical_record, decode_message_page,
        LogicalManifestRecord,
    };
    use crate::persistent_store::{snapshot, ConversationMutation};
    use rusqlite::params;
    use serde_json::json;

    fn open_j2_fixture() -> (tempfile::TempDir, PersistentStore, PayloadCas) {
        let directory = tempfile::tempdir().expect("create fixture directory");
        let store = PersistentStore::open(directory.path()).expect("open store");
        let cas = PayloadCas::new(directory.path()).expect("open payload CAS");
        (directory, store, cas)
    }

    fn seed_all_record_families(store: &PersistentStore, cas: &PayloadCas) -> (String, String) {
        let asset = cas
            .prepare_bytes(b"ordinary asset bytes")
            .expect("store asset");
        let inlay = cas
            .prepare_bytes(b"original inlay bytes")
            .expect("store inlay");
        let cold = cas.prepare_bytes(b"cold bytes").expect("store cold");
        let owner = cas
            .prepare_bytes(
                &encode_owner_manifest(&[OwnerManifestEntry {
                    tuple: [
                        "owner".to_owned(),
                        "assets/owner.bin".to_owned(),
                        "bin".to_owned(),
                    ],
                    payload_hash: None,
                }])
                .unwrap(),
            )
            .expect("store owner manifest");
        store
            .connection
            .execute(
                "UPDATE root SET value = ?1 WHERE generation = 'revision-0'",
                [r#"{"theme":"dark"}"#],
            )
            .expect("seed root");
        store.connection.execute(
            "INSERT INTO bot_presets (generation, preset_id, configured_index, name, image, value)
             VALUES ('revision-0', 'preset', 4, 'Preset', NULL, ?1)",
            [r#"{"name":"Preset"}"#],
        ).expect("seed preset");
        store
            .connection
            .execute(
                "INSERT INTO plugin_storage (generation, storage_key, byte_size, ordinal, value)
             VALUES ('revision-0', 'plugin', 7, 3, ?1)",
                [r#"{"x":1}"#],
            )
            .expect("seed plugin");
        store.connection.execute(
            "INSERT INTO characters (
                generation, character_id, configured_index, recent_at, trashed, name, image,
                conversation_count, type, creator_notes, trash_time, detail
             ) VALUES ('revision-0', 'char', 2, 10, 0, 'Char', NULL, 1, 'character', NULL, NULL, ?1)",
            [r#"{"chaId":"char","name":"Char","additionalAssets":[["owner","assets/owner.bin","bin"]]}"#],
        ).expect("seed character");
        store
            .connection
            .execute(
                "INSERT INTO conversations (
                generation, character_id, conversation_id, configured_index, recent_at,
                name, message_count, detail
             ) VALUES ('revision-0', 'char', 'chat', 1, 20, 'Chat', 129, ?1)",
                [r#"{"name":"Chat"}"#],
            )
            .expect("seed conversation");
        for index in 0..129_i64 {
            store
                .connection
                .execute(
                    "INSERT INTO messages (
                    generation, character_id, conversation_id, message_index, message_id, value
                 ) VALUES ('revision-0', 'char', 'chat', ?1, ?2, ?3)",
                    params![
                        index,
                        format!("m{index}"),
                        json!({"id": format!("m{index}")}).to_string()
                    ],
                )
                .expect("seed message");
        }
        store
            .connection
            .execute(
                "INSERT INTO asset_owner_heads (
                generation, owner_kind, owner_locator, present, manifest_hash, entry_count
             ) VALUES ('revision-0', 'character-additional-assets', 'char', 1, ?1, 1)",
                [owner.content_hash],
            )
            .expect("seed owner head");
        store
            .connection
            .execute(
                "INSERT INTO asset_aliases (
                generation, logical_key, object_hash, kind, size, mime, name, ext,
                inlay_type, width, height, metadata
             ) VALUES (
                'revision-0', 'same', ?1, 'asset', ?2,
                'application/octet-stream', 'asset.bin', 'bin', NULL, NULL, NULL, ?3
             ), (
                'revision-0', 'same', ?4, 'inlay', ?5,
                'image/webp', 'inlay.webp', 'webp', 'image', 320, 200, ?6
             )",
                params![
                    asset.content_hash,
                    i64::try_from(asset.byte_size).unwrap(),
                    r#"{"source":"asset-extra"}"#,
                    inlay.content_hash,
                    i64::try_from(inlay.byte_size).unwrap(),
                    r#"{"source":"inlay-extra"}"#,
                ],
            )
            .expect("seed asset and Inlay aliases");
        store
            .connection
            .execute(
                "INSERT INTO cold_aliases (generation, key, object_hash, size, metadata)
             VALUES ('revision-0', 'cold', ?1, ?2, ?3)",
                params![
                    cold.content_hash,
                    i64::try_from(cold.byte_size).unwrap(),
                    r#"{"scope":"module"}"#
                ],
            )
            .expect("seed cold alias");
        (asset.content_hash, inlay.content_hash)
    }

    fn seed_conversation(
        store: &PersistentStore,
        conversation_id: &str,
        configured_index: i64,
        message_count: i64,
    ) {
        store
            .connection
            .execute(
                "INSERT INTO conversations (
                    generation, character_id, conversation_id, configured_index, recent_at,
                    name, message_count, detail
                 ) VALUES ('revision-0', 'char', ?1, ?2, 30, ?1, ?3, ?4)",
                params![
                    conversation_id,
                    configured_index,
                    message_count,
                    json!({"name": conversation_id}).to_string(),
                ],
            )
            .expect("seed conversation");
        for index in 0..message_count {
            store
                .connection
                .execute(
                    "INSERT INTO messages (
                        generation, character_id, conversation_id, message_index,
                        message_id, value
                     ) VALUES ('revision-0', 'char', ?1, ?2, ?3, ?4)",
                    params![
                        conversation_id,
                        index,
                        format!("{conversation_id}-m{index}"),
                        json!({"id": format!("{conversation_id}-m{index}")}).to_string(),
                    ],
                )
                .expect("seed conversation message");
        }
        store
            .connection
            .execute(
                "UPDATE characters SET conversation_count = (
                    SELECT COUNT(*) FROM conversations
                    WHERE generation = 'revision-0' AND character_id = 'char'
                 ) WHERE generation = 'revision-0' AND character_id = 'char'",
                [],
            )
            .expect("refresh seeded conversation count");
    }

    fn replace_chat_messages(store: &mut PersistentStore, message_count: i64) {
        let transaction = store.connection.transaction().unwrap();
        transaction
            .execute(
                "DELETE FROM messages
                 WHERE generation = 'revision-0' AND character_id = 'char'
                   AND conversation_id = 'chat'",
                [],
            )
            .unwrap();
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO messages (
                        generation, character_id, conversation_id, message_index,
                        message_id, value
                     ) VALUES ('revision-0', 'char', 'chat', ?1, ?2, ?3)",
                )
                .unwrap();
            for index in 0..message_count {
                statement
                    .execute(params![
                        index,
                        format!("m{index}"),
                        json!({"id": format!("m{index}")}).to_string(),
                    ])
                    .unwrap();
            }
        }
        transaction
            .execute(
                "UPDATE conversations SET message_count = ?1
                 WHERE generation = 'revision-0' AND character_id = 'char'
                   AND conversation_id = 'chat'",
                [message_count],
            )
            .unwrap();
        transaction.commit().unwrap();
    }

    fn reconstructed_conversation_index(
        store: &PersistentStore,
        cas: &PayloadCas,
        built: &BuiltIndexedLogicalManifest,
        conversation_id: &str,
    ) -> u64 {
        let record = built
            .manifest
            .records
            .iter()
            .find_map(|record| match record {
                LogicalManifestRecord::Live(record) => {
                    match decode_logical_record_key(&record.key).unwrap() {
                        LogicalRecordLocator::Conversation {
                            character_id,
                            conversation_id: candidate,
                        } if character_id == "char" && candidate == conversation_id => Some(record),
                        _ => None,
                    }
                }
                _ => None,
            })
            .expect("conversation manifest record");
        let bytes = store
            .reconstruct_logical_object(
                cas,
                "library",
                &built.manifest.generation,
                &record.object_hash,
            )
            .expect("reconstruct canonical conversation envelope");
        let LogicalRecordEnvelope::Conversation {
            configured_index, ..
        } = decode_logical_record(&bytes).unwrap()
        else {
            panic!("expected conversation envelope");
        };
        configured_index
    }

    fn logical_generation_row_count(
        store: &PersistentStore,
        table: &str,
        generation_id: &str,
    ) -> i64 {
        store
            .connection
            .query_row(
                &format!(
                    "SELECT COUNT(*) FROM {table}
                     WHERE library_id = 'library' AND generation_id = ?1"
                ),
                [generation_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn assert_logical_generation_deleted(store: &PersistentStore, generation_id: &str) {
        for table in [
            "logical_record_dependencies",
            "logical_message_page_sources",
            "logical_record_heads",
            "logical_sync_generations",
        ] {
            assert_eq!(
                logical_generation_row_count(store, table, generation_id),
                0,
                "{table} retained rows for deleted logical generation"
            );
        }
    }

    #[test]
    fn rebuild_projects_every_family_and_reconstructs_pages_and_exact_payloads() {
        let (_directory, mut store, cas) = open_j2_fixture();
        let (asset_hash, inlay_hash) = seed_all_record_families(&store, &cas);
        let built = store
            .rebuild_logical_index(
                &cas,
                LogicalIndexBuildRequest {
                    library_id: "library".to_owned(),
                    generation_id: "generation-0".to_owned(),
                    generation_sequence: "0".to_owned(),
                    parent_generation_id: None,
                    lease: None,
                },
            )
            .expect("build logical index");

        let kinds: Vec<&str> = built
            .manifest
            .records
            .iter()
            .map(
                |record| match decode_logical_record_key(record.key()).unwrap() {
                    LogicalRecordLocator::Root => RECORD_KIND_ROOT,
                    LogicalRecordLocator::Preset { .. } => RECORD_KIND_PRESET,
                    LogicalRecordLocator::Plugin { .. } => RECORD_KIND_PLUGIN,
                    LogicalRecordLocator::Character { .. } => RECORD_KIND_CHARACTER,
                    LogicalRecordLocator::Conversation { .. } => RECORD_KIND_CONVERSATION,
                    LogicalRecordLocator::Asset { .. } => RECORD_KIND_ASSET,
                    LogicalRecordLocator::Inlay { .. } => RECORD_KIND_INLAY,
                    LogicalRecordLocator::Cold { .. } => RECORD_KIND_COLD,
                },
            )
            .collect();
        for expected in [
            RECORD_KIND_ROOT,
            RECORD_KIND_PRESET,
            RECORD_KIND_PLUGIN,
            RECORD_KIND_CHARACTER,
            RECORD_KIND_CONVERSATION,
            RECORD_KIND_ASSET,
            RECORD_KIND_INLAY,
            RECORD_KIND_COLD,
        ] {
            assert!(kinds.contains(&expected), "missing {expected}");
        }
        let page_rows: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM logical_message_page_sources
                 WHERE library_id = 'library' AND generation_id = 'generation-0'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(page_rows, 2);
        assert_eq!(
            cas.read_object(&built.manifest_hash).unwrap().unwrap(),
            built.manifest_bytes,
        );
        let base_manifest = cas.prepare_bytes(b"remote logical manifest").unwrap();
        store
            .connection
            .execute(
                "INSERT INTO logical_peer_common_bases (
                    peer_id, library_id, generation_id, manifest_hash,
                    generation_sequence, updated_at
                 ) VALUES ('peer', 'library', 'remote-generation', ?1, '1', 0)",
                [&base_manifest.content_hash],
            )
            .unwrap();
        let roots = snapshot::collect_asset_roots(&store.connection).unwrap();
        assert!(roots.object_hashes.contains(&built.manifest_hash));
        assert!(roots.object_hashes.contains(&base_manifest.content_hash));

        let conversation = built
            .manifest
            .records
            .iter()
            .find_map(|record| match record {
                LogicalManifestRecord::Live(record)
                    if matches!(
                        decode_logical_record_key(&record.key).unwrap(),
                        LogicalRecordLocator::Conversation { .. }
                    ) =>
                {
                    Some(record)
                }
                _ => None,
            })
            .unwrap();
        let record_bytes = store
            .reconstruct_logical_object(&cas, "library", "generation-0", &conversation.object_hash)
            .expect("reconstruct conversation record");
        let LogicalRecordEnvelope::Conversation {
            message_page_hashes,
            ..
        } = decode_logical_record(&record_bytes).unwrap()
        else {
            panic!("expected conversation envelope");
        };
        assert_eq!(message_page_hashes.len(), 2);
        assert_eq!(
            decode_message_page(
                &store
                    .reconstruct_logical_object(
                        &cas,
                        "library",
                        "generation-0",
                        &message_page_hashes[0],
                    )
                    .unwrap()
            )
            .unwrap()
            .len(),
            LOGICAL_MESSAGE_PAGE_SIZE,
        );
        assert_eq!(
            store
                .reconstruct_logical_object(&cas, "library", "generation-0", &asset_hash)
                .unwrap(),
            b"ordinary asset bytes",
        );
        assert_eq!(
            store
                .reconstruct_logical_object(&cas, "library", "generation-0", &inlay_hash)
                .unwrap(),
            b"original inlay bytes",
        );
        for record in &built.manifest.records {
            let LogicalManifestRecord::Live(record) = record else {
                continue;
            };
            let expected = match decode_logical_record_key(&record.key).unwrap() {
                LogicalRecordLocator::Asset { .. } => {
                    Some(("application/octet-stream", "asset-extra", None))
                }
                LogicalRecordLocator::Inlay { .. } => {
                    Some(("image/webp", "inlay-extra", Some("image")))
                }
                _ => None,
            };
            let Some((mime, source, inlay_type)) = expected else {
                continue;
            };
            let bytes = store
                .reconstruct_logical_object(&cas, "library", "generation-0", &record.object_hash)
                .unwrap();
            let metadata = match decode_logical_record(&bytes).unwrap() {
                LogicalRecordEnvelope::Asset { metadata, .. }
                | LogicalRecordEnvelope::Inlay { metadata, .. } => metadata,
                _ => unreachable!(),
            };
            let metadata = decode_asset_alias_metadata(&metadata).unwrap();
            assert_eq!(metadata.mime, mime);
            assert_eq!(metadata.inlay_type.as_deref(), inlay_type);
            assert_eq!(metadata.metadata["source"], source);
        }
        assert_eq!(
            store
                .build_indexed_logical_manifest("library", "generation-0")
                .unwrap()
                .manifest_hash,
            built.manifest_hash,
        );
    }

    #[test]
    fn projection_preserves_missing_payload_reference_size_without_dependency() {
        let (_directory, mut store, cas) = open_j2_fixture();
        store
            .connection
            .execute(
                "INSERT INTO asset_aliases (
                    generation, logical_key, object_hash, kind, size, mime, name, ext,
                    inlay_type, width, height, metadata
                 ) VALUES (
                    'revision-0', 'missing', NULL, 'asset', 99,
                    'application/octet-stream', 'missing.bin', 'bin',
                    NULL, NULL, NULL, '{\"legacy\":true}'
                 )",
                [],
            )
            .unwrap();
        let built = store
            .rebuild_logical_index(
                &cas,
                LogicalIndexBuildRequest {
                    library_id: "library".to_owned(),
                    generation_id: "generation-0".to_owned(),
                    generation_sequence: "0".to_owned(),
                    parent_generation_id: None,
                    lease: None,
                },
            )
            .unwrap();
        let record = built
            .manifest
            .records
            .iter()
            .find_map(|record| match record {
                LogicalManifestRecord::Live(record)
                    if matches!(
                        decode_logical_record_key(&record.key).unwrap(),
                        LogicalRecordLocator::Asset { .. }
                    ) =>
                {
                    Some(record)
                }
                _ => None,
            })
            .unwrap();
        assert!(record.dependencies.is_empty());
        let bytes = store
            .reconstruct_logical_object(&cas, "library", "generation-0", &record.object_hash)
            .unwrap();
        assert!(matches!(
            decode_logical_record(&bytes).unwrap(),
            LogicalRecordEnvelope::Asset {
                object_hash: None,
                size: 99,
                ..
            }
        ));
    }

    #[test]
    fn child_index_carries_explicit_tombstones_from_the_complete_parent() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        store
            .rebuild_logical_index(
                &cas,
                LogicalIndexBuildRequest {
                    library_id: "library".to_owned(),
                    generation_id: "generation-0".to_owned(),
                    generation_sequence: "0".to_owned(),
                    parent_generation_id: None,
                    lease: None,
                },
            )
            .unwrap();
        clone_pds_generation(&store.connection, "revision-0", "revision-1");
        store
            .connection
            .execute(
                "DELETE FROM bot_presets WHERE generation = 'revision-1' AND preset_id = 'preset'",
                [],
            )
            .unwrap();
        store
            .connection
            .execute_batch(
                "UPDATE meta SET value = '1' WHERE key = 'currentRevision';
                 UPDATE meta SET value = '\"revision-1\"' WHERE key = 'activeGeneration';",
            )
            .unwrap();

        let child = store
            .rebuild_logical_index(
                &cas,
                LogicalIndexBuildRequest {
                    library_id: "library".to_owned(),
                    generation_id: "generation-1".to_owned(),
                    generation_sequence: "1".to_owned(),
                    parent_generation_id: Some("generation-0".to_owned()),
                    lease: None,
                },
            )
            .unwrap();
        let preset_key = encode_logical_record_key(&LogicalRecordLocator::Preset {
            preset_id: "preset".to_owned(),
        })
        .unwrap();
        assert!(child.manifest.records.iter().any(|record| matches!(
            record,
            LogicalManifestRecord::Tombstone(tombstone)
                if tombstone.key == preset_key && tombstone.deleted_generation_sequence == "1"
        )));
    }

    #[test]
    fn projection_and_reads_fail_closed_for_old_or_incomplete_index_state() {
        let directory = tempfile::tempdir().unwrap();
        let mut old_store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        old_store
            .connection
            .execute_batch("ALTER TABLE asset_aliases DROP COLUMN metadata;")
            .unwrap();
        let error = old_store
            .rebuild_logical_index(
                &cas,
                LogicalIndexBuildRequest {
                    library_id: "library".to_owned(),
                    generation_id: "generation".to_owned(),
                    generation_sequence: "0".to_owned(),
                    parent_generation_id: None,
                    lease: None,
                },
            )
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("J2 asset_aliases column metadata"));

        let (_directory, store, cas) = open_j2_fixture();
        logical_schema::create_logical_schema(&store.connection).unwrap();
        store.connection.execute(
            "INSERT INTO logical_sync_generations (
                library_id, generation_id, generation_sequence, parent_generation_id,
                pds_generation, source_revision, state, manifest_hash, created_at, completed_at
             ) VALUES ('library', 'building', '0', NULL, 'revision-0', 0, 'building', NULL, 0, NULL)",
            [],
        ).unwrap();
        assert!(store
            .build_indexed_logical_manifest("library", "building")
            .is_err());
        assert!(store
            .reconstruct_logical_object(&cas, "library", "building", &"11".repeat(32),)
            .is_err());
    }

    fn logical_build_request() -> LogicalIndexBuildRequest {
        LogicalIndexBuildRequest {
            library_id: "library".to_owned(),
            generation_id: "generation-0".to_owned(),
            generation_sequence: "0".to_owned(),
            parent_generation_id: None,
            lease: None,
        }
    }

    fn root_commit(expected_revision: i64, value: Value) -> WorkingSetCommit {
        WorkingSetCommit {
            expected_revision,
            root: Some(value),
            replace_presets: None,
            character: None,
            character_details: None,
            replace_character: None,
            add_character: None,
            conversations: None,
            delete_character_id: None,
            plugin_storage: None,
            asset_owner_heads: None,
        }
    }

    #[test]
    fn atomic_asset_alias_batch_updates_the_incremental_logical_child() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        store
            .rebuild_logical_index(&cas, logical_build_request())
            .unwrap();
        let replacement = cas.prepare_bytes(b"replacement asset bytes").unwrap();
        let alias = AssetAlias {
            key: "same".to_owned(),
            object_hash: Some(replacement.content_hash.clone()),
            kind: "asset".to_owned(),
            size: i64::try_from(replacement.byte_size).unwrap(),
            mime: "application/octet-stream".to_owned(),
            name: "replacement.bin".to_owned(),
            ext: "bin".to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({ "source": "atomic-batch" }),
        };
        let mut commit = root_commit(0, json!({ "unused": true }));
        commit.root = None;

        store
            .commit_with_asset_aliases(&commit, std::slice::from_ref(&alias))
            .unwrap();
        let sealed = store.seal_active_logical_generation(&cas).unwrap();

        let record_key = encode_logical_record_key(&LogicalRecordLocator::Asset {
            logical_key: alias.key.clone(),
        })
        .unwrap();
        let (generation_id, state, object_hash): (String, String, String) = store
            .connection
            .query_row(
                "SELECT head.generation_id, head.state, head.object_hash
                 FROM logical_record_heads AS head
                 JOIN logical_library_head AS library
                   ON library.library_id = head.library_id
                  AND library.generation_id = head.generation_id
                 WHERE head.record_key = ?1",
                [record_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "live");
        assert_eq!(generation_id, sealed.manifest.generation);
        let bytes = store
            .reconstruct_logical_object(&cas, "library", &generation_id, &object_hash)
            .unwrap();
        let LogicalRecordEnvelope::Asset {
            object_hash,
            size,
            metadata,
        } = decode_logical_record(&bytes).unwrap()
        else {
            panic!("atomic alias batch must produce an asset record");
        };
        assert_eq!(
            object_hash.as_deref(),
            Some(replacement.content_hash.as_str())
        );
        assert_eq!(size, replacement.byte_size);
        let metadata = decode_asset_alias_metadata(&metadata).unwrap();
        assert_eq!(metadata.metadata["source"], "atomic-batch");
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM logical_record_dependencies
                     WHERE library_id = 'library' AND generation_id = ?1
                       AND record_key = ?2 AND object_hash = ?3 AND object_size = ?4",
                    params![
                        generation_id,
                        encode_logical_record_key(&LogicalRecordLocator::Asset {
                            logical_key: alias.key,
                        })
                        .unwrap(),
                        replacement.content_hash,
                        i64::try_from(replacement.byte_size).unwrap(),
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }

    #[test]
    fn building_generation_absorbs_repeated_saves_without_pds_copy_on_write() {
        let (_directory, mut store, cas) = open_j2_fixture();
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        logical_schema::reset_validation_count();

        store
            .commit(&root_commit(0, json!({"version": 1})))
            .unwrap();
        store
            .commit(&root_commit(1, json!({"version": 2})))
            .unwrap();

        assert_eq!(
            super::super::active_generation(&store.connection).unwrap(),
            "revision-0"
        );
        let row: (String, String, i64) = store
            .connection
            .query_row(
                "SELECT generation_id, pds_generation, source_revision
                 FROM logical_sync_generations WHERE state = 'building'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, ("generation-0".to_owned(), "revision-0".to_owned(), 2));
        assert_eq!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM root", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1,
        );
        assert_eq!(logical_schema::validation_count(), 0);
    }

    #[test]
    fn detached_snapshot_reader_allows_building_generation_to_save_in_place() {
        let (_directory, mut store, cas) = open_j2_fixture();
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        let lease = store.acquire_revision(0).unwrap().lease;

        store
            .commit(&root_commit(0, json!({"version": 1})))
            .unwrap();
        assert_eq!(
            super::super::active_generation(&store.connection).unwrap(),
            "revision-0"
        );
        let logical: (String, String, i64) = store
            .connection
            .query_row(
                "SELECT generation_id, pds_generation, source_revision
                 FROM logical_sync_generations WHERE state = 'building'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            logical,
            ("generation-0".to_owned(), "revision-0".to_owned(), 1)
        );
        store.release_revision(&lease).unwrap();

        store
            .commit(&root_commit(1, json!({"version": 2})))
            .unwrap();
        assert_eq!(
            super::super::active_generation(&store.connection).unwrap(),
            "revision-0"
        );
    }

    #[test]
    fn historical_lease_rebuild_cannot_replace_the_current_logical_head() {
        let (_directory, mut store, cas) = open_j2_fixture();
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        let lease = store.acquire_revision(0).unwrap().lease;
        store
            .commit(&root_commit(0, json!({"version": 1})))
            .unwrap();

        let error = store
            .rebuild_logical_index(
                &cas,
                LogicalIndexBuildRequest {
                    library_id: "library".to_owned(),
                    generation_id: "historical-generation".to_owned(),
                    generation_sequence: "1".to_owned(),
                    parent_generation_id: None,
                    lease: Some(lease.clone()),
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("detached revision"));
        let head: (String, String, i64) = store
            .connection
            .query_row(
                "SELECT generation.generation_id, generation.pds_generation,
                        generation.source_revision
                 FROM logical_library_head AS head
                 JOIN logical_sync_generations AS generation
                   ON generation.library_id = head.library_id
                  AND generation.generation_id = head.generation_id
                 WHERE head.singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            head,
            ("generation-0".to_owned(), "revision-0".to_owned(), 1)
        );
        let historical_rows: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM logical_sync_generations
                 WHERE generation_id = 'historical-generation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(historical_rows, 0);

        store.release_revision(&lease).unwrap();
        store
            .commit(&root_commit(1, json!({"version": 2})))
            .unwrap();
        assert_eq!(
            super::super::active_generation(&store.connection).unwrap(),
            "revision-0"
        );
    }

    #[test]
    fn sealing_causes_exactly_one_cow_and_repeated_seal_is_a_no_op() {
        let (_directory, mut store, cas) = open_j2_fixture();
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        let sealed = store.seal_active_logical_generation(&cas).unwrap();
        let sealed_again = store.seal_active_logical_generation(&cas).unwrap();
        assert_eq!(sealed_again.manifest_hash, sealed.manifest_hash);

        store
            .commit(&root_commit(0, json!({"version": 1})))
            .unwrap();
        assert_eq!(
            super::super::active_generation(&store.connection).unwrap(),
            "revision-1"
        );
        let child: (String, String, String, String) = store
            .connection
            .query_row(
                "SELECT generation_id, generation_sequence, parent_generation_id, state
                 FROM logical_sync_generations
                 WHERE generation_id != 'generation-0'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(child.1, "1");
        assert_eq!(child.2, "generation-0");
        assert_eq!(child.3, "building");

        store
            .commit(&root_commit(1, json!({"version": 2})))
            .unwrap();
        assert_eq!(
            super::super::active_generation(&store.connection).unwrap(),
            "revision-1"
        );
        let building_count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM logical_sync_generations WHERE state = 'building'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(building_count, 1);
    }

    #[test]
    fn full_replace_detaches_the_old_head_until_one_bounded_rebuild() {
        let (_directory, mut store, cas) = open_j2_fixture();
        store
            .rebuild_logical_index(&cas, logical_build_request())
            .unwrap();
        let staging = store.replace_begin().unwrap();
        store
            .replace_put_root(&staging.staging_id, &json!({"replacement": true}))
            .unwrap();
        store.replace_commit(&staging.staging_id, Some(0)).unwrap();

        assert!(!logical_index_is_active(&store.connection).unwrap());
        assert!(store
            .connection
            .query_row(
                "SELECT 1 FROM root WHERE generation = 'revision-0'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some());
        store
            .initialize_logical_index_building(
                &cas,
                LogicalIndexBuildRequest {
                    library_id: "library".to_owned(),
                    generation_id: "generation-1".to_owned(),
                    generation_sequence: "1".to_owned(),
                    parent_generation_id: Some("generation-0".to_owned()),
                    lease: None,
                },
            )
            .unwrap();
        let replacement = store.seal_active_logical_generation(&cas).unwrap();
        assert_eq!(
            replacement.manifest.parent_generation.as_deref(),
            Some("generation-0")
        );
        assert_eq!(replacement.manifest.source_revision, 1);
    }

    #[test]
    fn full_replace_detach_removes_building_generation_children() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        assert!(
            logical_generation_row_count(&store, "logical_record_dependencies", "generation-0") > 0
        );
        assert!(
            logical_generation_row_count(&store, "logical_message_page_sources", "generation-0")
                > 0
        );
        let staging = store.replace_begin().unwrap();
        store
            .replace_put_root(&staging.staging_id, &json!({"replacement": true}))
            .unwrap();

        store.replace_commit(&staging.staging_id, Some(0)).unwrap();

        assert_logical_generation_deleted(&store, "generation-0");
    }

    #[test]
    fn complete_generation_reconstructs_after_lease_release_reopen_and_sweep() {
        let (directory, mut store, cas) = open_j2_fixture();
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        let sealed = store.seal_active_logical_generation(&cas).unwrap();
        let root = sealed
            .manifest
            .records
            .iter()
            .find_map(|record| match record {
                LogicalManifestRecord::Live(record)
                    if matches!(
                        decode_logical_record_key(&record.key).unwrap(),
                        LogicalRecordLocator::Root
                    ) =>
                {
                    Some(record.object_hash.clone())
                }
                _ => None,
            })
            .unwrap();
        let session = store
            .pin_logical_generation("library", "generation-0")
            .unwrap();
        let lease = store.acquire_revision(0).unwrap().lease;
        store
            .commit(&root_commit(0, json!({"version": 1})))
            .unwrap();
        store.release_revision(&lease).unwrap();
        drop(store);

        let mut reopened = PersistentStore::open(directory.path()).unwrap();
        snapshot::sweep_temporary_generations(&mut reopened.connection).unwrap();
        reopened
            .resume_logical_generation_pin(&session, "library", "generation-0")
            .unwrap();
        reopened
            .resume_logical_generation_pin(&session, "library", "generation-0")
            .unwrap();
        let mismatch = reopened
            .resume_logical_generation_pin(&session, "other-library", "generation-0")
            .unwrap_err();
        assert!(mismatch.to_string().contains("tuple does not match"));
        assert!(!reopened
            .reconstruct_logical_object(&cas, "library", "generation-0", &root)
            .unwrap()
            .is_empty());
        let pin_count: i64 = reopened
            .connection
            .query_row(
                "SELECT COUNT(*) FROM logical_generation_session_pins WHERE session_id = ?1",
                [session.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pin_count, 1);
        reopened.release_logical_generation_pin(&session).unwrap();
        reopened
            .prune_logical_generation("library", "generation-0")
            .unwrap();
        assert!(reopened
            .connection
            .query_row(
                "SELECT 1 FROM root WHERE generation = 'revision-0'",
                [],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_none());
    }

    #[test]
    fn session_pin_blocks_explicit_generation_prune() {
        let (_directory, mut store, cas) = open_j2_fixture();
        store
            .rebuild_logical_index(&cas, logical_build_request())
            .unwrap();
        let session = store
            .pin_logical_generation("library", "generation-0")
            .unwrap();
        store
            .commit(&root_commit(0, json!({"version": 1})))
            .unwrap();
        let error = store
            .prune_logical_generation("library", "generation-0")
            .unwrap_err();
        assert!(error.to_string().contains("session-pinned"));
        store.release_logical_generation_pin(&session).unwrap();
        store
            .prune_logical_generation("library", "generation-0")
            .unwrap();
    }

    #[test]
    fn prune_removes_generation_children_with_foreign_keys_disabled() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        store
            .rebuild_logical_index(&cas, logical_build_request())
            .unwrap();
        store
            .commit(&root_commit(0, json!({"version": 1})))
            .unwrap();
        assert!(
            logical_generation_row_count(&store, "logical_record_dependencies", "generation-0") > 0
        );
        assert!(
            logical_generation_row_count(&store, "logical_message_page_sources", "generation-0")
                > 0
        );

        store
            .prune_logical_generation("library", "generation-0")
            .unwrap();

        assert_logical_generation_deleted(&store, "generation-0");
    }

    #[test]
    fn range_append_rehashes_only_the_intersecting_128_message_page() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        let key = encode_logical_record_key(&LogicalRecordLocator::Conversation {
            character_id: "char".to_owned(),
            conversation_id: "chat".to_owned(),
        })
        .unwrap();
        let before: Vec<(i64, String)> = store
            .connection
            .prepare(
                "SELECT rowid, object_hash FROM logical_message_page_sources
                 WHERE library_id = 'library' AND generation_id = 'generation-0'
                   AND record_key = ?1 ORDER BY page_index",
            )
            .unwrap()
            .query_map([key.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let mut commit = root_commit(0, json!({"theme": "dark"}));
        commit.root = None;
        commit.conversations = Some(vec![ConversationMutation::ReplaceRange {
            character_id: "char".to_owned(),
            conversation_id: "chat".to_owned(),
            start: 129,
            delete_count: 0,
            messages: vec![json!({"chatId": "m129", "data": "new"})],
            conversation: None,
            configured_index: None,
        }]);
        store.commit(&commit).unwrap();
        let after: Vec<(i64, String)> = store
            .connection
            .prepare(
                "SELECT rowid, object_hash FROM logical_message_page_sources
                 WHERE library_id = 'library' AND generation_id = 'generation-0'
                   AND record_key = ?1 ORDER BY page_index",
            )
            .unwrap()
            .query_map([key.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(after.len(), 2);
        assert_eq!(after[0], before[0]);
        assert_ne!(after[1].1, before[1].1);
    }

    #[test]
    fn two_appends_in_one_commit_produce_final_canonical_pages_and_manifest() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        let mut commit = root_commit(0, json!({"unused": true}));
        commit.root = None;
        commit.conversations = Some(vec![
            ConversationMutation::ReplaceRange {
                character_id: "char".to_owned(),
                conversation_id: "chat".to_owned(),
                start: 129,
                delete_count: 0,
                messages: vec![json!({"chatId": "m129", "data": "first"})],
                conversation: None,
                configured_index: None,
            },
            ConversationMutation::ReplaceRange {
                character_id: "char".to_owned(),
                conversation_id: "chat".to_owned(),
                start: 130,
                delete_count: 0,
                messages: vec![json!({"chatId": "m130", "data": "second"})],
                conversation: None,
                configured_index: None,
            },
        ]);

        store.commit(&commit).unwrap();
        let expected_page = encode_message_page(&[
            json!({"id": "m128"}),
            json!({"chatId": "m129", "data": "first"}),
            json!({"chatId": "m130", "data": "second"}),
        ])
        .unwrap();
        let page: (String, i64) = store
            .connection
            .query_row(
                "SELECT object_hash, message_count
                 FROM logical_message_page_sources
                 WHERE library_id = 'library' AND generation_id = 'generation-0'
                   AND page_index = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(page, (expected_page.hash.clone(), 3));

        let sealed = store.seal_active_logical_generation(&cas).unwrap();
        let conversation = sealed
            .manifest
            .records
            .iter()
            .find_map(|record| match record {
                LogicalManifestRecord::Live(record)
                    if matches!(
                        decode_logical_record_key(&record.key).unwrap(),
                        LogicalRecordLocator::Conversation { .. }
                    ) =>
                {
                    Some(record)
                }
                _ => None,
            })
            .unwrap();
        assert!(conversation
            .dependencies
            .iter()
            .any(|dependency| dependency == &expected_page.hash));
        assert_eq!(
            store
                .build_indexed_logical_manifest("library", "generation-0")
                .unwrap()
                .manifest_hash,
            sealed.manifest_hash,
        );
    }

    #[test]
    fn conversation_change_coalescing_preserves_disjoint_ranges_and_earliest_tail() {
        let change = |old_count, new_count, fixed_page_ranges, tail_from_page, was_new| {
            ConversationChange::ReplaceRange {
                character_id: "char".to_owned(),
                conversation_id: "chat".to_owned(),
                old_count,
                new_count,
                fixed_page_ranges,
                tail_from_page,
                was_new,
            }
        };

        let mut fixed = Vec::new();
        push_conversation_change(&mut fixed, change(640, 640, vec![(0, 1)], None, false)).unwrap();
        push_conversation_change(&mut fixed, change(640, 640, vec![(4, 5)], None, false)).unwrap();
        let ConversationChange::ReplaceRange {
            fixed_page_ranges,
            tail_from_page,
            ..
        } = &fixed[0]
        else {
            panic!("expected fixed replacement");
        };
        assert_eq!(fixed_page_ranges, &vec![(0, 1), (4, 5)]);
        assert_eq!(*tail_from_page, None);

        let mut mixed = Vec::new();
        push_conversation_change(&mut mixed, change(640, 640, vec![(0, 1)], None, false)).unwrap();
        push_conversation_change(&mut mixed, change(640, 641, Vec::new(), Some(3), false)).unwrap();
        push_conversation_change(&mut mixed, change(641, 641, vec![(4, 5)], None, false)).unwrap();
        push_conversation_change(&mut mixed, change(641, 641, vec![(1, 2)], None, false)).unwrap();
        let ConversationChange::ReplaceRange {
            fixed_page_ranges,
            tail_from_page,
            ..
        } = &mixed[0]
        else {
            panic!("expected mixed replacement");
        };
        assert_eq!(fixed_page_ranges, &vec![(0, 2)]);
        assert_eq!(*tail_from_page, Some(3));

        let mut recreated = vec![ConversationChange::Delete {
            character_id: "char".to_owned(),
            conversation_id: "chat".to_owned(),
        }];
        push_conversation_change(&mut recreated, change(0, 1, Vec::new(), Some(0), true)).unwrap();
        let ConversationChange::ReplaceRange {
            was_new,
            tail_from_page,
            ..
        } = &recreated[0]
        else {
            panic!("expected recreated replacement");
        };
        assert!(*was_new);
        assert_eq!(*tail_from_page, Some(0));
    }

    #[test]
    fn inserted_conversation_refreshes_shifted_sibling_envelopes_without_body_reads() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        seed_conversation(&store, "sibling", 0, 1);
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        reset_message_page_rehash_reads();
        let mut commit = root_commit(0, json!({"unused": true}));
        commit.root = None;
        commit.conversations = Some(vec![ConversationMutation::ReplaceRange {
            character_id: "char".to_owned(),
            conversation_id: "inserted".to_owned(),
            start: 0,
            delete_count: 0,
            messages: Vec::new(),
            conversation: Some(json!({"name": "Inserted", "lastDate": 40})),
            configured_index: Some(0),
        }]);

        store.commit(&commit).unwrap();
        assert!(take_message_page_rehash_reads().is_empty());
        let sealed = store.seal_active_logical_generation(&cas).unwrap();
        assert_eq!(
            reconstructed_conversation_index(&store, &cas, &sealed, "sibling"),
            1
        );
        assert_eq!(
            reconstructed_conversation_index(&store, &cas, &sealed, "chat"),
            2
        );
    }

    #[test]
    fn delete_then_recreate_refreshes_shifted_siblings_without_normalizing_indices() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        seed_conversation(&store, "first", 0, 1);
        seed_conversation(&store, "sibling", 2, 1);
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        reset_message_page_rehash_reads();
        let mut commit = root_commit(0, json!({"unused": true}));
        commit.root = None;
        commit.conversations = Some(vec![
            ConversationMutation::Delete {
                character_id: "char".to_owned(),
                conversation_id: "first".to_owned(),
            },
            ConversationMutation::ReplaceRange {
                character_id: "char".to_owned(),
                conversation_id: "first".to_owned(),
                start: 0,
                delete_count: 0,
                messages: vec![json!({"id": "first-recreated"})],
                conversation: Some(json!({"name": "First recreated", "lastDate": 50})),
                configured_index: Some(0),
            },
        ]);

        store.commit(&commit).unwrap();
        assert!(take_message_page_rehash_reads().is_empty());
        let sealed = store.seal_active_logical_generation(&cas).unwrap();
        assert_eq!(
            reconstructed_conversation_index(&store, &cas, &sealed, "chat"),
            2
        );
        assert_eq!(
            reconstructed_conversation_index(&store, &cas, &sealed, "sibling"),
            3
        );
    }

    #[test]
    fn distant_fixed_replacements_rehash_only_disjoint_message_pages() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        replace_chat_messages(&mut store, 640);
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        let key = encode_logical_record_key(&LogicalRecordLocator::Conversation {
            character_id: "char".to_owned(),
            conversation_id: "chat".to_owned(),
        })
        .unwrap();
        let read_pages = |store: &PersistentStore| {
            store
                .connection
                .prepare(
                    "SELECT page_index, rowid, object_hash
                     FROM logical_message_page_sources
                     WHERE library_id = 'library' AND generation_id = 'generation-0'
                       AND record_key = ?1 ORDER BY page_index",
                )
                .unwrap()
                .query_map([key.as_str()], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let before = read_pages(&store);
        assert_eq!(before.len(), 5);
        reset_message_page_rehash_reads();
        let mut commit = root_commit(0, json!({"unused": true}));
        commit.root = None;
        commit.conversations = Some(vec![
            ConversationMutation::ReplaceRange {
                character_id: "char".to_owned(),
                conversation_id: "chat".to_owned(),
                start: 1,
                delete_count: 1,
                messages: vec![json!({"id": "first-page-replacement"})],
                conversation: None,
                configured_index: None,
            },
            ConversationMutation::ReplaceRange {
                character_id: "char".to_owned(),
                conversation_id: "chat".to_owned(),
                start: 513,
                delete_count: 1,
                messages: vec![json!({"id": "last-page-replacement"})],
                conversation: None,
                configured_index: None,
            },
        ]);

        store.commit(&commit).unwrap();
        assert_eq!(
            take_message_page_rehash_reads(),
            vec![
                ("char".to_owned(), "chat".to_owned(), 0),
                ("char".to_owned(), "chat".to_owned(), 4),
            ]
        );
        let after = read_pages(&store);
        assert_eq!(&after[1..4], &before[1..4]);
        assert_ne!(after[0].2, before[0].2);
        assert_ne!(after[4].2, before[4].2);

        let sealed = store.seal_active_logical_generation(&cas).unwrap();
        let middle = decode_message_page(
            &store
                .reconstruct_logical_object(&cas, "library", "generation-0", &after[2].2)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(middle.len(), LOGICAL_MESSAGE_PAGE_SIZE);
        assert_eq!(middle[0], json!({"id": "m256"}));
        assert_eq!(
            store
                .build_indexed_logical_manifest("library", "generation-0")
                .unwrap()
                .manifest_hash,
            sealed.manifest_hash,
        );
    }

    #[test]
    fn mixed_fixed_and_length_changes_keep_earlier_ranges_and_rebuild_only_the_tail() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        replace_chat_messages(&mut store, 640);
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        reset_message_page_rehash_reads();
        let mut commit = root_commit(0, json!({"unused": true}));
        commit.root = None;
        commit.conversations = Some(vec![
            ConversationMutation::ReplaceRange {
                character_id: "char".to_owned(),
                conversation_id: "chat".to_owned(),
                start: 1,
                delete_count: 1,
                messages: vec![json!({"id": "early-fixed"})],
                conversation: None,
                configured_index: None,
            },
            ConversationMutation::ReplaceRange {
                character_id: "char".to_owned(),
                conversation_id: "chat".to_owned(),
                start: 384,
                delete_count: 0,
                messages: vec![json!({"id": "tail-insertion"})],
                conversation: None,
                configured_index: None,
            },
            ConversationMutation::ReplaceRange {
                character_id: "char".to_owned(),
                conversation_id: "chat".to_owned(),
                start: 513,
                delete_count: 1,
                messages: vec![json!({"id": "covered-fixed"})],
                conversation: None,
                configured_index: None,
            },
        ]);

        store.commit(&commit).unwrap();
        assert_eq!(
            take_message_page_rehash_reads(),
            vec![
                ("char".to_owned(), "chat".to_owned(), 0),
                ("char".to_owned(), "chat".to_owned(), 3),
                ("char".to_owned(), "chat".to_owned(), 4),
                ("char".to_owned(), "chat".to_owned(), 5),
            ]
        );
        let sealed = store.seal_active_logical_generation(&cas).unwrap();
        assert_eq!(
            store
                .build_indexed_logical_manifest("library", "generation-0")
                .unwrap()
                .manifest_hash,
            sealed.manifest_hash,
        );
    }

    #[test]
    fn deleting_an_asset_alias_clones_the_retained_generation_and_records_a_tombstone() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        store
            .rebuild_logical_index(&cas, logical_build_request())
            .unwrap();

        let deleted = store.delete_asset_alias("asset", "same", 0).unwrap();

        assert_eq!(deleted.revision, 1);
        assert_eq!(active_generation(&store.connection).unwrap(), "revision-1");
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM asset_aliases
                     WHERE generation = 'revision-0' AND kind = 'asset' AND logical_key = 'same'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM asset_aliases
                     WHERE generation = 'revision-1' AND kind = 'asset' AND logical_key = 'same'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
        let asset_key = encode_logical_record_key(&LogicalRecordLocator::Asset {
            logical_key: "same".to_owned(),
        })
        .unwrap();
        let tombstone: (String, String) = store
            .connection
            .query_row(
                "SELECT head.state, head.deleted_generation_sequence
                 FROM logical_record_heads AS head
                 JOIN logical_library_head AS library
                   ON library.library_id = head.library_id
                  AND library.generation_id = head.generation_id
                 WHERE head.record_key = ?1",
                [asset_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(tombstone, ("tombstone".to_owned(), "1".to_owned()));
    }

    #[test]
    fn child_commit_records_plugin_deletion_as_tombstone() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        store
            .rebuild_logical_index(&cas, logical_build_request())
            .unwrap();
        let mut commit = root_commit(0, json!({"theme": "dark"}));
        commit.root = None;
        commit.plugin_storage = Some(vec![PluginStorageMutation::Delete {
            key: "plugin".to_owned(),
        }]);
        store.commit(&commit).unwrap();
        let plugin_key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
            storage_key: "plugin".to_owned(),
        })
        .unwrap();
        let tombstone: (String, String) = store
            .connection
            .query_row(
                "SELECT state, deleted_generation_sequence
                 FROM logical_record_heads
                 JOIN logical_library_head USING (library_id, generation_id)
                 WHERE record_key = ?1",
                [plugin_key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(tombstone, ("tombstone".to_owned(), "1".to_owned()));
    }

    #[test]
    fn plugin_clear_removes_existing_dependencies_before_parent_tombstone() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        let dependency = cas.prepare_bytes(b"plugin dependency").unwrap();
        let plugin_key = encode_logical_record_key(&LogicalRecordLocator::Plugin {
            storage_key: "plugin".to_owned(),
        })
        .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO logical_record_dependencies (
                    library_id, generation_id, record_key, object_hash, object_size
                 ) VALUES ('library', 'generation-0', ?1, ?2, ?3)",
                params![
                    plugin_key,
                    dependency.content_hash,
                    i64::try_from(dependency.byte_size).unwrap(),
                ],
            )
            .unwrap();
        store.seal_active_logical_generation(&cas).unwrap();
        let mut commit = root_commit(0, json!({"unused": true}));
        commit.root = None;
        commit.plugin_storage = Some(vec![PluginStorageMutation::Clear]);

        store.commit(&commit).unwrap();

        let dependency_count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM logical_record_dependencies AS dependency
                 JOIN logical_library_head AS head
                   ON head.library_id = dependency.library_id
                  AND head.generation_id = dependency.generation_id
                 WHERE dependency.record_key = ?1",
                [plugin_key.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dependency_count, 0);
        store.seal_active_logical_generation(&cas).unwrap();
    }

    #[test]
    fn child_conversation_deletion_removes_page_children_before_tombstone() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        store
            .rebuild_logical_index(&cas, logical_build_request())
            .unwrap();
        let mut commit = root_commit(0, json!({"unused": true}));
        commit.root = None;
        commit.conversations = Some(vec![ConversationMutation::Delete {
            character_id: "char".to_owned(),
            conversation_id: "chat".to_owned(),
        }]);

        store.commit(&commit).unwrap();
        let conversation_key = encode_logical_record_key(&LogicalRecordLocator::Conversation {
            character_id: "char".to_owned(),
            conversation_id: "chat".to_owned(),
        })
        .unwrap();
        let tombstone: (String, String) = store
            .connection
            .query_row(
                "SELECT state, deleted_generation_sequence
                 FROM logical_record_heads
                 JOIN logical_library_head USING (library_id, generation_id)
                 WHERE record_key = ?1",
                [conversation_key.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(tombstone, ("tombstone".to_owned(), "1".to_owned()));
        for table in [
            "logical_record_dependencies",
            "logical_message_page_sources",
        ] {
            let count: i64 = store
                .connection
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM {table} AS child
                         JOIN logical_library_head AS head
                           ON head.library_id = child.library_id
                          AND head.generation_id = child.generation_id
                         WHERE child.record_key = ?1"
                    ),
                    [conversation_key.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 0, "{table} retained tombstone children");
        }
        store.seal_active_logical_generation(&cas).unwrap();
    }

    #[test]
    fn startup_cleanup_removes_abandoned_generation_children() {
        let (_directory, mut store, cas) = open_j2_fixture();
        seed_all_record_families(&store, &cas);
        store
            .initialize_logical_index_building(&cas, logical_build_request())
            .unwrap();
        clone_pds_generation(&store.connection, "revision-0", "staging-logical-abandoned");
        store
            .connection
            .execute(
                "UPDATE logical_sync_generations
                 SET pds_generation = 'staging-logical-abandoned'
                 WHERE library_id = 'library' AND generation_id = 'generation-0'",
                [],
            )
            .unwrap();
        store
            .connection
            .execute("DELETE FROM logical_library_head WHERE singleton = 1", [])
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO snapshot_leases (lease, generation, revision, created_at)
                 VALUES ('snapshot-staged', 'staging-logical-abandoned', 0, 0)",
                [],
            )
            .unwrap();
        assert!(
            logical_generation_row_count(&store, "logical_record_dependencies", "generation-0") > 0
        );
        assert!(
            logical_generation_row_count(&store, "logical_message_page_sources", "generation-0")
                > 0
        );

        cleanup_abandoned_logical_staging(&mut store.connection).unwrap();
        snapshot::sweep_temporary_generations(&mut store.connection).unwrap();
        let remaining: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM root WHERE generation = 'staging-logical-abandoned'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
        assert_logical_generation_deleted(&store, "generation-0");
    }

    fn clone_pds_generation(connection: &Connection, from: &str, to: &str) {
        for (table, columns) in [
            ("root", "value"),
            (
                "bot_presets",
                "preset_id, configured_index, name, image, value",
            ),
            (
                "plugin_storage",
                "storage_key, byte_size, ordinal, value",
            ),
            (
                "characters",
                "character_id, configured_index, recent_at, trashed, name, image, conversation_count, type, creator_notes, trash_time, detail",
            ),
            (
                "conversations",
                "character_id, conversation_id, configured_index, recent_at, name, message_count, detail",
            ),
            (
                "messages",
                "character_id, conversation_id, message_index, message_id, value",
            ),
            (
                "asset_aliases",
                "logical_key, object_hash, kind, size, mime, name, ext, inlay_type, width, height, metadata",
            ),
            (
                "asset_owner_heads",
                "owner_kind, owner_locator, present, manifest_hash, entry_count",
            ),
            ("cold_aliases", "key, object_hash, size, metadata"),
        ] {
            connection
                .execute(
                    &format!(
                        "INSERT INTO {table} (generation, {columns}) \
                         SELECT ?2, {columns} FROM {table} WHERE generation = ?1"
                    ),
                    params![from, to],
                )
                .unwrap();
        }
    }
}
