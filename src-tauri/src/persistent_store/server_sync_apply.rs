//! Semantic validation precedes the transaction which activates remote records.
//! Cursor/base advancement and local outbox acknowledgement share that transaction.
use super::record_apply as rows;
use super::{
    active_generation, current_revision,
    server_sync_outbox::{acknowledge_keys, ServerDirtyKey},
    server_sync_projection::{preserve_local_view, ServerPayload},
    PersistentStore, StoreError, StoreResult,
};
use crate::{
    asset_repository::{
        owner_manifest_codec::{decode_owner_manifest, encode_owner_manifest},
        PayloadCas,
    },
    logical_records::{
        decode_asset_alias_metadata, decode_logical_record_key, encode_logical_record,
        encode_logical_record_key, LogicalRecordEnvelope, LogicalRecordLocator,
    },
};
use risunest_sync_wire::{Domain, RecordVersion, RemoteHead};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use std::collections::BTreeSet;

mod staging;
pub(crate) use staging::ValidatedRecords;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct RemoteRecord {
    pub key: String,
    pub version: RecordVersion,
    pub payload: Option<ServerPayload>,
    pub local_hash: Option<String>,
}
#[derive(Clone)]
pub(crate) struct ValidatedRecord {
    record: RemoteRecord,
    locator: LogicalRecordLocator,
}

impl ValidatedRecord {
    pub(crate) fn into_parts(self) -> (RemoteRecord, LogicalRecordLocator) {
        (self.record, self.locator)
    }
}

pub(crate) struct ReplicaAdvance {
    pub scope_clears: Vec<(String, String)>,
    pub publish_keys: Vec<ServerDirtyKey>,
    pub clear_revision: Option<i64>,
    pub bases: Vec<(Domain, String, RecordVersion, Option<String>)>,
    pub finish_operation: bool,
    pub scanned_revision: Option<i64>,
    pub applied_sections: Vec<Domain>,
}
impl Default for ReplicaAdvance {
    fn default() -> Self {
        Self {
            scope_clears: Vec::new(),
            publish_keys: Vec::new(),
            clear_revision: None,
            bases: Vec::new(),
            finish_operation: false,
            scanned_revision: None,
            applied_sections: Vec::new(),
        }
    }
}
fn invalid<T>(message: &str) -> StoreResult<T> {
    Err(StoreError::Validation {
        message: message.into(),
    })
}
fn semantic(_: StoreError) -> StoreError {
    StoreError::Validation {
        message: "Server record failed semantic validation".into(),
    }
}

/// Requires dependencies in verified local CAS before returning a record that
/// can be applied. Error messages never include received payload strings.
pub(crate) fn validate_remote(
    record: RemoteRecord,
    cas: &PayloadCas,
) -> StoreResult<ValidatedRecord> {
    validate_remote_with_residency(record, cas, |_, _| Ok(false))
}
pub(crate) fn validate_remote_with_residency(
    mut record: RemoteRecord,
    cas: &PayloadCas,
    remote: impl Fn(&str, Option<u64>) -> StoreResult<bool>,
) -> StoreResult<ValidatedRecord> {
    record
        .version
        .validate()
        .map_err(|_| StoreError::Validation {
            message: "Invalid server record version".into(),
        })?;
    let locator = decode_logical_record_key(&record.key).map_err(|_| StoreError::Validation {
        message: "Invalid server record key".into(),
    })?;
    match (&record.version, &mut record.payload) {
        (RecordVersion::Live { .. }, Some(payload)) => {
            encode_logical_record(&payload.record).map_err(|_| StoreError::Validation {
                message: "Invalid server record envelope".into(),
            })?;
            rows::validate_locator_envelope(&locator, &payload.record).map_err(semantic)?;
            if matches!(locator, LogicalRecordLocator::Conversation { .. })
                != payload.messages.is_some()
            {
                return invalid("Server message sequence differs from record family");
            }
            match &mut payload.record {
                LogicalRecordEnvelope::Root { value, owner_heads }
                | LogicalRecordEnvelope::Character {
                    detail: value,
                    owner_heads,
                    ..
                } => {
                    let mut resolved = Vec::with_capacity(owner_heads.len());
                    for head in owner_heads.iter() {
                        let tuples = if let Some(hash) = &head.manifest_hash {
                            let bytes =
                                cas.read_object(hash)?
                                    .ok_or_else(|| StoreError::Validation {
                                        message: "Missing server owner manifest".into(),
                                    })?;
                            if risunest_sync_wire::hash(&bytes) != *hash {
                                return invalid("Server owner manifest hash mismatch");
                            }
                            let entries = decode_owner_manifest(&bytes).map_err(|_| {
                                StoreError::Validation {
                                    message: "Invalid server owner manifest".into(),
                                }
                            })?;
                            if entries.len() as u64 != head.entry_count
                                || encode_owner_manifest(&entries).map_err(|_| {
                                    StoreError::Validation {
                                        message: "Invalid server owner manifest".into(),
                                    }
                                })? != bytes
                            {
                                return invalid("Server owner manifest differs from its head");
                            }
                            let mut tuples = Vec::with_capacity(entries.len());
                            for entry in entries {
                                if let Some(hash) = entry.payload_hash {
                                    if cas.stat_object(&hex::encode(hash))?.is_none()
                                        && !remote(&hex::encode(hash), None)?
                                    {
                                        return invalid("Missing server owner payload");
                                    }
                                }
                                tuples.push(Value::Array(
                                    entry.tuple.into_iter().map(Value::String).collect(),
                                ));
                            }
                            Some(tuples)
                        } else {
                            None
                        };
                        resolved.push(rows::ResolvedOwnerHead {
                            head: head.clone(),
                            tuples,
                        });
                    }
                    if let LogicalRecordLocator::Character { character_id } = &locator {
                        rows::rehydrate_character_owner(value, character_id, &resolved)
                            .map_err(semantic)?;
                    } else {
                        rows::rehydrate_root_owners(value, &resolved).map_err(semantic)?;
                    }
                }
                LogicalRecordEnvelope::ArchivedCharacter {
                    archive_object_hash,
                    archive_object_size,
                    asset_hashes,
                    owner_heads,
                    ..
                } => {
                    if cas.stat_object(archive_object_hash)? != Some(*archive_object_size)
                        && !remote(archive_object_hash, Some(*archive_object_size))?
                    {
                        return invalid("Missing server archive object");
                    }
                    for hash in asset_hashes {
                        if cas.stat_object(hash)?.is_none() && !remote(hash, None)? {
                            return invalid("Missing server archived asset");
                        }
                    }
                    for head in owner_heads.iter() {
                        let Some(hash) = &head.manifest_hash else {
                            continue;
                        };
                        let bytes = cas.read_object(hash)?.ok_or_else(|| {
                            StoreError::Validation {
                                message: "Missing server owner manifest".into(),
                            }
                        })?;
                        if risunest_sync_wire::hash(&bytes) != *hash {
                            return invalid("Server owner manifest hash mismatch");
                        }
                        let entries = decode_owner_manifest(&bytes).map_err(|_| {
                            StoreError::Validation {
                                message: "Invalid server owner manifest".into(),
                            }
                        })?;
                        if entries.len() as u64 != head.entry_count
                            || encode_owner_manifest(&entries).map_err(|_| {
                                StoreError::Validation {
                                    message: "Invalid server owner manifest".into(),
                                }
                            })? != bytes
                        {
                            return invalid("Server owner manifest differs from its head");
                        }
                        for entry in entries {
                            if let Some(hash) = entry.payload_hash {
                                let hash = hex::encode(hash);
                                if cas.stat_object(&hash)?.is_none() && !remote(&hash, None)? {
                                    return invalid("Missing server owner payload");
                                }
                            }
                        }
                    }
                }
                LogicalRecordEnvelope::Conversation {
                    message_page_hashes,
                    ..
                } => {
                    if !message_page_hashes.is_empty() {
                        return invalid(
                            "Server conversations must carry their ordered message sequence",
                        );
                    }
                }
                LogicalRecordEnvelope::Asset {
                    object_hash,
                    size,
                    metadata,
                }
                | LogicalRecordEnvelope::Inlay {
                    object_hash,
                    size,
                    metadata,
                } => {
                    if let Some(hash) = object_hash {
                        if cas.stat_object(hash)? != Some(*size) && !remote(hash, Some(*size))? {
                            return invalid("Server payload size differs from alias");
                        }
                    }
                    let typed =
                        decode_asset_alias_metadata(metadata).map_err(|_| StoreError::Validation {
                            message: "Invalid server alias metadata".into(),
                        })?;
                    if matches!(locator, LogicalRecordLocator::Asset { .. })
                        && (typed.inlay_type.is_some()
                            || typed.width.is_some()
                            || typed.height.is_some())
                    {
                        return invalid("Asset contains inlay-only metadata");
                    }
                    if matches!(locator, LogicalRecordLocator::Inlay { .. })
                        && typed.inlay_type.is_none()
                    {
                        return invalid("Inlay type is missing");
                    }
                }
                // The store no longer holds cold payloads; the shared record format still carries the variant.
                LogicalRecordEnvelope::Cold { .. } => {
                    return invalid("Cold records are unsupported")
                }
                _ => (),
            }
        }
        (RecordVersion::Tombstone { .. } | RecordVersion::Absent, None) => {
            if matches!(locator, LogicalRecordLocator::Root) {
                return invalid("Server cannot delete the root");
            }
        }
        _ => return invalid("Server version and payload disagree"),
    }
    Ok(ValidatedRecord { record, locator })
}

impl PersistentStore {
    #[cfg(test)]
    pub(crate) fn server_apply(
        &mut self,
        expected_revision: i64,
        expected_head: Option<&RemoteHead>,
        next_head: &RemoteHead,
        records: Vec<ValidatedRecord>,
        acknowledged: &[ServerDirtyKey],
        scopes: &[(String, String)],
    ) -> StoreResult<i64> {
        let mut staged = ValidatedRecords::new()?;
        for record in records {
            staged.push(record)?;
        }
        self.server_apply_advance(
            expected_revision,
            expected_head,
            next_head,
            &staged,
            acknowledged,
            scopes,
            ReplicaAdvance::default(),
        )
    }
    pub(crate) fn server_apply_advance(
        &mut self,
        expected_revision: i64,
        expected_head: Option<&RemoteHead>,
        next_head: &RemoteHead,
        records: &ValidatedRecords,
        acknowledged: &[ServerDirtyKey],
        scopes: &[(String, String)],
        advance: ReplicaAdvance,
    ) -> StoreResult<i64> {
        next_head.validate().map_err(|_| StoreError::Validation {
            message: "Invalid server cursor".into(),
        })?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::sync_selection::require_server(&tx)?;
        let actual = current_revision(&tx)?;
        if actual != expected_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_revision,
                actual,
            });
        }
        let stored: Option<String> = tx.query_row(
            "SELECT head FROM server_sync_state WHERE singleton=1 AND registration_required=0",
            [],
            |r| r.get(0),
        )?;
        let stored: Option<RemoteHead> = stored.map(|s| serde_json::from_str(&s)).transpose()?;
        if stored.as_ref() != expected_head {
            return invalid("Server replica cursor changed during apply");
        }
        if let Some(previous) = expected_head {
            if previous.library_id != next_head.library_id
                || previous.epoch != next_head.epoch
                || next_head.seq < previous.seq
            {
                return invalid("Server replica requires epoch reconciliation");
            }
        }
        let active = active_generation(&tx)?;
        records.visit(true, |item| {
            if let (LogicalRecordLocator::Character { character_id }, None) =
                (&item.locator, &item.record.payload)
            {
                let mut statement=tx.prepare("SELECT conversation_id FROM conversations WHERE generation=?1 AND character_id=?2")?;
                let mut children = statement.query(params![active, character_id])?;
                while let Some(child) = children.next()? {
                    let key = encode_logical_record_key(&LogicalRecordLocator::Conversation {
                        character_id: character_id.clone(),
                        conversation_id: child.get(0)?,
                    })
                    .map_err(|_| StoreError::Validation {
                        message: "Invalid local conversation key".into(),
                    })?;
                    if !records.deletes(&key)? {
                        return invalid(
                            "Server character deletion would discard a preserved conversation",
                        );
                    }
                }
            }
            Ok(())
        })?;
        let revision = if records.is_empty() {
            actual
        } else {
            actual
                .checked_add(1)
                .ok_or_else(|| StoreError::Validation {
                    message: "Local revision overflow".into(),
                })?
        };
        let generation = active;
        super::content_change_index::begin_mutation(&tx, &generation, revision, "server")?;
        let mut touched = BTreeSet::new();
        records.visit(false, |item| {
            if let LogicalRecordLocator::Character { character_id }
            | LogicalRecordLocator::Conversation { character_id, .. } = &item.locator
            {
                touched.insert(character_id.clone());
            }
            if let Some(mut payload) = item.record.payload {
                let raw_character: Option<String> =
                    if let LogicalRecordLocator::Character { character_id } = &item.locator {
                        tx.query_row(
                            "SELECT detail FROM characters WHERE generation=?1 AND character_id=?2",
                            params![generation, character_id],
                            |r| r.get(0),
                        )
                        .optional()?
                    } else {
                        None
                    };
                let raw_root: Option<String> = if matches!(item.locator, LogicalRecordLocator::Root)
                {
                    tx.query_row(
                        "SELECT value FROM root WHERE generation=?1",
                        [&generation],
                        |r| r.get(0),
                    )
                    .optional()?
                } else {
                    None
                };
                let raw_character: Option<Value> = raw_character
                    .map(|s| serde_json::from_str(&s))
                    .transpose()?;
                let raw_root: Option<Value> =
                    raw_root.map(|s| serde_json::from_str(&s)).transpose()?;
                preserve_local_view(&mut payload, raw_character.as_ref(), raw_root.as_ref());
                rows::apply_materialized_record(
                    &tx,
                    &generation,
                    item.locator,
                    payload.record,
                    payload.messages.as_deref(),
                )
                .map_err(semantic)?;
            } else {
                rows::apply_delete(&tx, &generation, &item.locator).map_err(semantic)?;
            }
            tx.execute("INSERT INTO server_sync_base(domain,key,version,local_hash) VALUES('library',?1,?2,?3) ON CONFLICT(domain,key) DO UPDATE SET version=excluded.version,local_hash=excluded.local_hash",params![item.record.key,serde_json::to_string(&item.record.version)?,item.record.local_hash])?;
            Ok(())
        })?;
        for id in touched {
            tx.execute("UPDATE characters SET conversation_count=(SELECT count(*) FROM conversations WHERE generation=?1 AND character_id=?2) WHERE generation=?1 AND character_id=?2",params![generation,id])?;
        }
        rows::validate_configured_index_uniqueness(&tx, &generation).map_err(semantic)?;
        let duplicate_plugins:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM plugin_storage WHERE generation=?1 GROUP BY ordinal HAVING count(*)>1)",[&generation],|r|r.get(0))?;
        if duplicate_plugins {
            return invalid("Server plugin order contains duplicate positions");
        }
        acknowledge_keys(&tx, acknowledged)?;
        // Explicit conflict choices can create an outgoing intent for a key
        // which had no local edit (for example rejecting a newly added remote
        // key after clear). Persist it before the prepared worker can disappear.
        for key in advance.publish_keys {
            tx.execute("INSERT INTO server_sync_dirty VALUES(?1,?2,?3,?4) ON CONFLICT(kind,key1,key2) DO UPDATE SET revision=max(revision,excluded.revision)",params![key.kind,key.key1,key.key2,key.revision])?;
        }
        if let Some(revision) = advance.clear_revision {
            tx.execute(
                "DELETE FROM server_sync_clears WHERE revision<=?1",
                [revision],
            )?;
        }
        for (domain, key, version, local_hash) in advance.bases {
            tx.execute("INSERT INTO server_sync_base VALUES(?1,?2,?3,?4) ON CONFLICT(domain,key) DO UPDATE SET version=excluded.version,local_hash=excluded.local_hash",params![domain.as_str(),key,serde_json::to_string(&version)?,local_hash])?;
        }
        if advance.finish_operation {
            let (phase, operation_revision): (String, i64) = tx.query_row(
                "SELECT phase,local_revision FROM server_sync_operation WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let receipt: risunest_sync_wire::Receipt = serde_json::from_str(&phase)?;
            if receipt.status != risunest_sync_wire::TerminalStatus::Committed
                || receipt.head.epoch != next_head.epoch
                || receipt.head.seq > next_head.seq
            {
                return invalid("Pending receipt is not included in the applied server revision");
            }
            tx.execute("DELETE FROM server_sync_dirty WHERE EXISTS(SELECT 1 FROM server_sync_operation_records AS sent WHERE sent.kind=server_sync_dirty.kind AND sent.key1=server_sync_dirty.key1 AND sent.key2=server_sync_dirty.key2 AND sent.revision>=server_sync_dirty.revision)",[])?;
            tx.execute(
                "DELETE FROM server_sync_clears WHERE revision<=?1",
                [operation_revision],
            )?;
            for table in [
                "server_sync_operation",
                "server_sync_operation_records",
                "server_sync_operation_pages",
                "server_sync_operation_scopes",
            ] {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
            }
        }
        // Only the sections this activation reconciled release their work set.
        // A section left unreceived keeps its marks for the next cycle.
        let reconciled = advance
            .applied_sections
            .iter()
            .map(|domain| format!("'{}'", domain.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        tx.execute(
            &format!("DELETE FROM server_sync_remote_dirty WHERE domain IN ({reconciled})"),
            [],
        )?;
        tx.execute("UPDATE server_sync_state SET reconciling=0", [])?;
        if advance.scanned_revision == Some(actual) {
            tx.execute(
                "UPDATE server_sync_state SET full_scan=0 WHERE singleton=1",
                [],
            )?;
            tx.execute(
                "DELETE FROM server_sync_dirty WHERE kind='full' AND revision<=?1",
                [actual],
            )?;
        }
        for (scope, version) in scopes {
            tx.execute("INSERT INTO server_sync_scope_base VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET version=excluded.version",params![scope,version])?;
        }
        for (scope, version) in advance.scope_clears {
            tx.execute("INSERT INTO server_sync_scope_clear_base VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET version=excluded.version",params![scope,version])?;
        }
        tx.execute(
            "UPDATE server_sync_state SET head=?1 WHERE singleton=1",
            [serde_json::to_string(next_head)?],
        )?;
        // Only sections this cycle applied advance. The rest stay unreceived.
        for domain in advance.applied_sections {
            let section = next_head
                .section(domain)
                .map_err(|_| StoreError::Validation {
                    message: "Invalid server cursor".into(),
                })?;
            tx.execute("INSERT INTO server_sync_remote_sections VALUES(?1,?2,?3) ON CONFLICT(domain) DO UPDATE SET applied_seq=excluded.applied_seq,state_id=excluded.state_id",params![domain.as_str(),next_head.seq.as_str(),section.state_id])?;
        }
        super::content_change_index::finish_mutation(&tx)?;
        super::commit::set_active(&tx, revision, &generation)?;
        tx.commit()?;
        Ok(revision)
    }
}
