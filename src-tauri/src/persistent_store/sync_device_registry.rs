use super::{
    active_generation, current_revision, logical_index::scan_compact_manifest, PersistentStore,
    StoreError, StoreResult,
};
use crate::peer_sync::logical_delta::{
    hash_logical_manifest, validate_logical_manifest, LogicalManifest,
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_DEVICE_ID_CHARS: usize = 1_024;
const MAX_GENERATION_SEQUENCE_DIGITS: usize = 64;
const MAX_TOMBSTONE_PAGE_LIMIT: i64 = 4_096;
const MAX_CURSOR_BYTES: usize = 140_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncGenerationIdentity {
    pub(crate) generation_id: String,
    pub(crate) manifest_hash: String,
    pub(crate) generation_sequence: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum RegisteredSyncDeviceStatus {
    Active,
    Revoked,
    Forgotten,
}

impl RegisteredSyncDeviceStatus {
    fn parse(value: &str) -> StoreResult<Self> {
        match value {
            "active" => Ok(Self::Active),
            "revoked" => Ok(Self::Revoked),
            "forgotten" => Ok(Self::Forgotten),
            _ => validation("sync device registry contains an unknown status"),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RegisteredSyncDevice {
    pub(crate) library_id: String,
    pub(crate) device_id: String,
    pub(crate) status: RegisteredSyncDeviceStatus,
    pub(crate) acknowledged_generation: SyncGenerationIdentity,
    pub(crate) registered_at: i64,
    pub(crate) acknowledged_at: i64,
    pub(crate) revoked_at: Option<i64>,
    pub(crate) forgotten_at: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SyncDeviceAckState {
    pub(crate) shared_identity: SyncGenerationIdentity,
    pub(crate) local_identity: SyncGenerationIdentity,
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedSyncDeviceRegistration {
    library_id: String,
    device_id: String,
    acknowledged_generation: SyncGenerationIdentity,
    registered_at: i64,
}

impl VerifiedSyncDeviceRegistration {
    pub(crate) fn from_authenticated_p5_receipt(
        library_id: &str,
        device_id: &str,
        acknowledged_generation: SyncGenerationIdentity,
        registered_at: i64,
    ) -> StoreResult<Self> {
        validate_library_id(library_id)?;
        validate_device_id(device_id)?;
        validate_identity(&acknowledged_generation)?;
        if registered_at < 0 {
            return validation("sync device registration timestamp must be nonnegative");
        }
        Ok(Self {
            library_id: library_id.to_owned(),
            device_id: device_id.to_owned(),
            acknowledged_generation,
            registered_at,
        })
    }

    #[cfg(test)]
    pub(super) fn for_test(
        library_id: &str,
        device_id: &str,
        acknowledged_generation: SyncGenerationIdentity,
        registered_at: i64,
    ) -> Self {
        Self {
            library_id: library_id.to_owned(),
            device_id: device_id.to_owned(),
            acknowledged_generation,
            registered_at,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TombstoneCollectionItem {
    pub(crate) record_key: String,
    pub(crate) deleted_generation_sequence: String,
    pub(crate) blocking_device_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TombstoneCollectionPage {
    pub(crate) generation: SyncGenerationIdentity,
    pub(crate) items: Vec<TombstoneCollectionItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_cursor: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TombstoneCursor {
    generation: SyncGenerationIdentity,
    last_record_key: String,
}

impl PersistentStore {
    pub(crate) fn sync_device_ack_state(
        &self,
        library_id: &str,
        device_id: &str,
    ) -> StoreResult<SyncDeviceAckState> {
        validate_library_id(library_id)?;
        validate_device_id(device_id)?;
        let device = load_device(&self.connection, library_id, device_id)?.ok_or_else(|| {
            StoreError::Validation {
                message: "sync device is not registered".to_owned(),
            }
        })?;
        if device.status != RegisteredSyncDeviceStatus::Active {
            return sync_conflict("only an active sync device has a usable acknowledgement");
        }
        let shared_identity = device.acknowledged_generation;
        if load_common_base(&self.connection, library_id, device_id)?.as_ref()
            != Some(&shared_identity)
        {
            return validation("sync device acknowledgement differs from its common base");
        }
        let proof = load_ack_proof(&self.connection, library_id, device_id)?.ok_or_else(|| {
            StoreError::Validation {
                message: "sync device acknowledgement proof is missing".to_owned(),
            }
        })?;
        require_ack_proof(
            &self.connection,
            library_id,
            device_id,
            &shared_identity,
            &proof.local_identity,
        )?;
        Ok(SyncDeviceAckState {
            shared_identity,
            local_identity: proof.local_identity,
        })
    }

    pub(crate) fn verify_shared_ack_local_proof(
        &self,
        shared_identity: &SyncGenerationIdentity,
        shared_manifest: &LogicalManifest,
        local_identity: &SyncGenerationIdentity,
    ) -> StoreResult<VerifiedSharedAckLocalProof> {
        validate_identity(shared_identity)?;
        validate_identity(local_identity)?;
        validate_logical_manifest(shared_manifest).map_err(|error| StoreError::Validation {
            message: error.to_string(),
        })?;
        let shared_hash =
            hash_logical_manifest(shared_manifest).map_err(|error| StoreError::Validation {
                message: error.to_string(),
            })?;
        if shared_manifest.generation != shared_identity.generation_id
            || shared_manifest.generation_sequence != shared_identity.generation_sequence
            || shared_hash != shared_identity.manifest_hash
        {
            return validation("shared acknowledgement identity does not match its manifest");
        }
        let local = scan_compact_manifest(
            &self.connection,
            &shared_manifest.library_id,
            &local_identity.generation_id,
            true,
        )?;
        if local.manifest_hash != local_identity.manifest_hash
            || local.manifest.generation_sequence != local_identity.generation_sequence
        {
            return validation("local acknowledgement witness identity is stale");
        }
        if local.manifest.schema != shared_manifest.schema
            || local.manifest.library_id != shared_manifest.library_id
            || local.manifest.records != shared_manifest.records
            || local.manifest.objects != shared_manifest.objects
        {
            return validation("local acknowledgement witness content differs from shared content");
        }
        Ok(VerifiedSharedAckLocalProof {
            library_id: shared_manifest.library_id.clone(),
            shared_identity: shared_identity.clone(),
            local_identity: local_identity.clone(),
            verified_at: now_millis()?,
        })
    }

    pub(crate) fn register_verified_sync_device(
        &mut self,
        receipt: VerifiedSyncDeviceRegistration,
        expected_local_revision: i64,
    ) -> StoreResult<RegisteredSyncDevice> {
        validate_library_id(&receipt.library_id)?;
        validate_device_id(&receipt.device_id)?;
        validate_identity(&receipt.acknowledged_generation)?;
        if receipt.registered_at < 0 {
            return validation("sync device registration timestamp must be nonnegative");
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actual_revision = current_revision(&transaction)?;
        if actual_revision != expected_local_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_local_revision,
                actual: actual_revision,
            });
        }
        require_complete_generation(
            &transaction,
            &receipt.library_id,
            &receipt.acknowledged_generation,
        )?;
        let existing: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM logical_sync_devices
                WHERE library_id = ?1 AND device_id = ?2
             )",
            params![receipt.library_id, receipt.device_id],
            |row| row.get(0),
        )?;
        if existing {
            return sync_conflict("sync device identity is already registered");
        }
        let common_base_exists: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM logical_peer_common_bases
                WHERE peer_id = ?1 AND library_id = ?2
             )",
            params![receipt.device_id, receipt.library_id],
            |row| row.get(0),
        )?;
        if common_base_exists {
            return sync_conflict("sync device common base already exists without a registry row");
        }

        transaction.execute(
            "INSERT INTO logical_peer_common_bases (
                peer_id, library_id, generation_id, manifest_hash,
                generation_sequence, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                receipt.device_id,
                receipt.library_id,
                receipt.acknowledged_generation.generation_id,
                receipt.acknowledged_generation.manifest_hash,
                receipt.acknowledged_generation.generation_sequence,
                receipt.registered_at,
            ],
        )?;
        transaction.execute(
            "INSERT INTO logical_sync_devices (
                library_id, device_id, status,
                acknowledged_generation_id, acknowledged_manifest_hash,
                acknowledged_generation_sequence,
                registered_at, acknowledged_at, revoked_at, forgotten_at
             ) VALUES (?1, ?2, 'active', ?3, ?4, ?5, ?6, ?6, NULL, NULL)",
            params![
                receipt.library_id,
                receipt.device_id,
                receipt.acknowledged_generation.generation_id,
                receipt.acknowledged_generation.manifest_hash,
                receipt.acknowledged_generation.generation_sequence,
                receipt.registered_at,
            ],
        )?;
        insert_exact_local_proof(
            &transaction,
            &receipt.library_id,
            &receipt.device_id,
            &receipt.acknowledged_generation,
            receipt.registered_at,
        )?;
        let registered = load_device(&transaction, &receipt.library_id, &receipt.device_id)?
            .ok_or_else(|| StoreError::Store {
                message: "registered sync device disappeared before commit".to_owned(),
            })?;
        transaction.commit()?;
        Ok(registered)
    }

    pub(crate) fn attach_verified_sync_device_at_common_base(
        &mut self,
        receipt: VerifiedSyncDeviceRegistration,
        expected_local_revision: i64,
    ) -> StoreResult<RegisteredSyncDevice> {
        validate_library_id(&receipt.library_id)?;
        validate_device_id(&receipt.device_id)?;
        validate_identity(&receipt.acknowledged_generation)?;
        if receipt.registered_at < 0 {
            return validation("sync device registration timestamp must be nonnegative");
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actual_revision = current_revision(&transaction)?;
        if actual_revision != expected_local_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_local_revision,
                actual: actual_revision,
            });
        }
        require_complete_generation(
            &transaction,
            &receipt.library_id,
            &receipt.acknowledged_generation,
        )?;
        if let Some(existing) = load_device(&transaction, &receipt.library_id, &receipt.device_id)?
        {
            require_device_and_common_base(
                &transaction,
                &receipt.library_id,
                &receipt.device_id,
                &receipt.acknowledged_generation,
            )?;
            if existing.status != RegisteredSyncDeviceStatus::Active {
                return sync_conflict("inactive sync device cannot be registered for P5");
            }
            require_ack_proof(
                &transaction,
                &receipt.library_id,
                &receipt.device_id,
                &receipt.acknowledged_generation,
                &receipt.acknowledged_generation,
            )?;
            return Ok(existing);
        }
        require_common_base_identity(
            &transaction,
            &receipt.library_id,
            &receipt.device_id,
            &receipt.acknowledged_generation,
        )?;
        transaction.execute(
            "INSERT INTO logical_sync_devices (
                library_id, device_id, status,
                acknowledged_generation_id, acknowledged_manifest_hash,
                acknowledged_generation_sequence,
                registered_at, acknowledged_at, revoked_at, forgotten_at
             ) VALUES (?1, ?2, 'active', ?3, ?4, ?5, ?6, ?6, NULL, NULL)",
            params![
                receipt.library_id,
                receipt.device_id,
                receipt.acknowledged_generation.generation_id,
                receipt.acknowledged_generation.manifest_hash,
                receipt.acknowledged_generation.generation_sequence,
                receipt.registered_at,
            ],
        )?;
        insert_exact_local_proof(
            &transaction,
            &receipt.library_id,
            &receipt.device_id,
            &receipt.acknowledged_generation,
            receipt.registered_at,
        )?;
        let attached = load_device(&transaction, &receipt.library_id, &receipt.device_id)?
            .ok_or_else(|| StoreError::Store {
                message: "attached sync device disappeared before commit".to_owned(),
            })?;
        transaction.commit()?;
        Ok(attached)
    }

    pub(crate) fn advance_sync_device_ack(
        &mut self,
        library_id: &str,
        device_id: &str,
        expected_previous: &SyncGenerationIdentity,
        next: &SyncGenerationIdentity,
    ) -> StoreResult<RegisteredSyncDevice> {
        validate_library_id(library_id)?;
        validate_device_id(device_id)?;
        validate_identity(expected_previous)?;
        validate_identity(next)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = advance_sync_device_ack_in_transaction(
            &transaction,
            library_id,
            device_id,
            expected_previous,
            next,
        )?;
        transaction.commit()?;
        Ok(updated)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn advance_sync_device_shared_ack(
        &mut self,
        library_id: &str,
        device_id: &str,
        expected_previous_shared: &SyncGenerationIdentity,
        expected_previous_local: &SyncGenerationIdentity,
        next_shared: &SyncGenerationIdentity,
        next_proof: VerifiedSharedAckLocalProof,
    ) -> StoreResult<RegisteredSyncDevice> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let updated = advance_sync_device_shared_ack_in_transaction(
            &transaction,
            library_id,
            device_id,
            expected_previous_shared,
            expected_previous_local,
            next_shared,
            next_proof,
        )?;
        transaction.commit()?;
        Ok(updated)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn advance_active_sync_device_shared_ack(
        &mut self,
        library_id: &str,
        device_id: &str,
        expected_local_revision: i64,
        expected_previous_shared: &SyncGenerationIdentity,
        expected_previous_local: &SyncGenerationIdentity,
        next_shared: &SyncGenerationIdentity,
        shared_manifest: &LogicalManifest,
        local_identity: &SyncGenerationIdentity,
    ) -> StoreResult<RegisteredSyncDevice> {
        if expected_local_revision < 0 {
            return validation("expected local revision must be nonnegative");
        }
        let proof =
            self.verify_shared_ack_local_proof(next_shared, shared_manifest, local_identity)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actual_revision = current_revision(&transaction)?;
        if actual_revision != expected_local_revision {
            return Err(StoreError::RevisionConflict {
                expected: expected_local_revision,
                actual: actual_revision,
            });
        }
        let active = active_generation(&transaction)?;
        let local_is_active: bool = transaction.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM logical_sync_generations
                WHERE library_id = ?1 AND generation_id = ?2
                  AND manifest_hash = ?3 AND generation_sequence = ?4
                  AND pds_generation = ?5 AND source_revision = ?6
                  AND state = 'complete' AND completed_at IS NOT NULL
             )",
            params![
                library_id,
                local_identity.generation_id,
                local_identity.manifest_hash,
                local_identity.generation_sequence,
                active,
                expected_local_revision,
            ],
            |row| row.get(0),
        )?;
        if !local_is_active {
            return validation(
                "local acknowledgement witness is not the exact active logical generation",
            );
        }
        let updated = advance_sync_device_shared_ack_in_transaction(
            &transaction,
            library_id,
            device_id,
            expected_previous_shared,
            expected_previous_local,
            next_shared,
            proof,
        )?;
        transaction.commit()?;
        Ok(updated)
    }

    pub(crate) fn revoke_sync_device(
        &mut self,
        library_id: &str,
        device_id: &str,
        expected_previous: &SyncGenerationIdentity,
    ) -> StoreResult<RegisteredSyncDevice> {
        validate_library_id(library_id)?;
        validate_device_id(device_id)?;
        validate_identity(expected_previous)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current =
            require_device_and_common_base(&transaction, library_id, device_id, expected_previous)?;
        if current.status == RegisteredSyncDeviceStatus::Forgotten {
            return sync_conflict("forgotten sync device cannot be revoked");
        }
        if current.status == RegisteredSyncDeviceStatus::Revoked {
            return Ok(current);
        }
        let revoked_at = current.registered_at.max(now_millis()?);
        let changed = transaction.execute(
            "UPDATE logical_sync_devices
             SET status = 'revoked', revoked_at = ?1
             WHERE library_id = ?2 AND device_id = ?3 AND status = 'active'
               AND acknowledged_generation_id = ?4
               AND acknowledged_manifest_hash = ?5
               AND acknowledged_generation_sequence = ?6",
            params![
                revoked_at,
                library_id,
                device_id,
                expected_previous.generation_id,
                expected_previous.manifest_hash,
                expected_previous.generation_sequence,
            ],
        )?;
        if changed != 1 {
            return sync_conflict("sync device revoke state changed concurrently");
        }
        let revoked =
            load_device(&transaction, library_id, device_id)?.ok_or_else(|| StoreError::Store {
                message: "revoked sync device disappeared before commit".to_owned(),
            })?;
        transaction.commit()?;
        Ok(revoked)
    }

    pub(crate) fn forget_sync_device(
        &mut self,
        library_id: &str,
        device_id: &str,
        expected_previous: &SyncGenerationIdentity,
    ) -> StoreResult<RegisteredSyncDevice> {
        validate_library_id(library_id)?;
        validate_device_id(device_id)?;
        validate_identity(expected_previous)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = load_device(&transaction, library_id, device_id)?.ok_or_else(|| {
            StoreError::Validation {
                message: "sync device is not registered".to_owned(),
            }
        })?;
        if current.status == RegisteredSyncDeviceStatus::Forgotten {
            if current.acknowledged_generation != *expected_previous {
                return sync_conflict(
                    "forgotten sync device identity does not match expected state",
                );
            }
            let common_base_exists: bool = transaction.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM logical_peer_common_bases
                    WHERE peer_id = ?1 AND library_id = ?2
                 )",
                params![device_id, library_id],
                |row| row.get(0),
            )?;
            if common_base_exists {
                return sync_conflict("forgotten sync device still has a common base");
            }
            let proof_exists: bool = transaction.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM logical_sync_device_ack_proofs
                    WHERE library_id = ?1 AND device_id = ?2
                 )",
                params![library_id, device_id],
                |row| row.get(0),
            )?;
            if proof_exists {
                return sync_conflict("forgotten sync device still has an acknowledgement proof");
            }
            return Ok(current);
        }
        require_device_and_common_base(&transaction, library_id, device_id, expected_previous)?;
        let forgotten_at = current.registered_at.max(now_millis()?);
        let device_updated = transaction.execute(
            "UPDATE logical_sync_devices
             SET status = 'forgotten', forgotten_at = ?1
             WHERE library_id = ?2 AND device_id = ?3
               AND status IN ('active', 'revoked')
               AND acknowledged_generation_id = ?4
               AND acknowledged_manifest_hash = ?5
               AND acknowledged_generation_sequence = ?6",
            params![
                forgotten_at,
                library_id,
                device_id,
                expected_previous.generation_id,
                expected_previous.manifest_hash,
                expected_previous.generation_sequence,
            ],
        )?;
        let common_deleted = transaction.execute(
            "DELETE FROM logical_peer_common_bases
             WHERE peer_id = ?1 AND library_id = ?2
               AND generation_id = ?3 AND manifest_hash = ?4
               AND generation_sequence = ?5",
            params![
                device_id,
                library_id,
                expected_previous.generation_id,
                expected_previous.manifest_hash,
                expected_previous.generation_sequence,
            ],
        )?;
        let proof_deleted = transaction.execute(
            "DELETE FROM logical_sync_device_ack_proofs
             WHERE library_id = ?1 AND device_id = ?2
               AND shared_generation_id = ?3 AND shared_manifest_hash = ?4
               AND shared_generation_sequence = ?5",
            params![
                library_id,
                device_id,
                expected_previous.generation_id,
                expected_previous.manifest_hash,
                expected_previous.generation_sequence,
            ],
        )?;
        if device_updated != 1 || common_deleted != 1 || proof_deleted != 1 {
            return sync_conflict("sync device forget state changed concurrently");
        }
        let forgotten =
            load_device(&transaction, library_id, device_id)?.ok_or_else(|| StoreError::Store {
                message: "forgotten sync device disappeared before commit".to_owned(),
            })?;
        transaction.commit()?;
        Ok(forgotten)
    }

    pub(crate) fn list_sync_devices(
        &self,
        library_id: &str,
    ) -> StoreResult<Vec<RegisteredSyncDevice>> {
        validate_library_id(library_id)?;
        let mut statement = self.connection.prepare(
            "SELECT library_id, device_id, status,
                    acknowledged_generation_id, acknowledged_manifest_hash,
                    acknowledged_generation_sequence,
                    registered_at, acknowledged_at, revoked_at, forgotten_at
             FROM logical_sync_devices
             WHERE library_id = ?1
             ORDER BY device_id",
        )?;
        let rows = statement
            .query_map([library_id], decode_device_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter().map(validate_device_row).collect()
    }

    pub(crate) fn plan_tombstone_collection(
        &mut self,
        expected_generation: &SyncGenerationIdentity,
        cursor: Option<&str>,
        limit: i64,
    ) -> StoreResult<TombstoneCollectionPage> {
        validate_identity(expected_generation)?;
        if !(1..=MAX_TOMBSTONE_PAGE_LIMIT).contains(&limit) {
            return validation("tombstone collection page limit must be between 1 and 4096");
        }
        let decoded_cursor = cursor.map(decode_cursor).transpose()?;
        if let Some(cursor) = &decoded_cursor {
            if cursor.generation != *expected_generation {
                return sync_conflict("tombstone collection cursor generation is stale");
            }
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        require_active_head(&transaction, expected_generation)?;
        validate_registry_consistency(&transaction, expected_generation)?;
        let devices = list_plan_devices(&transaction, expected_generation)?;
        let after = decoded_cursor
            .as_ref()
            .map(|cursor| cursor.last_record_key.as_str())
            .unwrap_or("");
        let mut statement = transaction.prepare(
            "SELECT record_key, deleted_generation_sequence
             FROM logical_record_heads
             WHERE library_id = ?1 AND generation_id = ?2
               AND state = 'tombstone' AND record_key > ?3
             ORDER BY record_key
             LIMIT ?4",
        )?;
        let mut tombstones = statement
            .query_map(
                params![
                    active_library_id(&transaction)?,
                    expected_generation.generation_id,
                    after,
                    limit + 1,
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let has_more = tombstones.len() > limit as usize;
        if has_more {
            tombstones.pop();
        }
        let items = tombstones
            .into_iter()
            .map(|(record_key, deleted_generation_sequence)| {
                validate_sequence(&deleted_generation_sequence)?;
                let blocking_device_ids = devices
                    .iter()
                    .filter(|device| match device.status {
                        RegisteredSyncDeviceStatus::Active => {
                            compare_sequences(
                                &device.local_generation_sequence,
                                &deleted_generation_sequence,
                            ) != std::cmp::Ordering::Greater
                        }
                        RegisteredSyncDeviceStatus::Revoked => true,
                        RegisteredSyncDeviceStatus::Forgotten => false,
                    })
                    .map(|device| device.device_id.clone())
                    .collect();
                Ok(TombstoneCollectionItem {
                    record_key,
                    deleted_generation_sequence,
                    blocking_device_ids,
                })
            })
            .collect::<StoreResult<Vec<_>>>()?;
        let next_cursor = if has_more {
            items
                .last()
                .map(|item| {
                    encode_cursor(&TombstoneCursor {
                        generation: expected_generation.clone(),
                        last_record_key: item.record_key.clone(),
                    })
                })
                .transpose()?
        } else {
            None
        };
        drop(transaction);
        Ok(TombstoneCollectionPage {
            generation: expected_generation.clone(),
            items,
            next_cursor,
        })
    }
}

#[derive(Clone, Debug)]
pub(crate) struct VerifiedSharedAckLocalProof {
    library_id: String,
    shared_identity: SyncGenerationIdentity,
    local_identity: SyncGenerationIdentity,
    verified_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StoredSyncDeviceAckProof {
    shared_identity: SyncGenerationIdentity,
    local_identity: SyncGenerationIdentity,
    verified_at: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TombstonePlanDevice {
    device_id: String,
    status: RegisteredSyncDeviceStatus,
    local_generation_sequence: String,
}

pub(super) fn advance_sync_device_ack_in_transaction(
    connection: &Connection,
    library_id: &str,
    device_id: &str,
    expected_previous: &SyncGenerationIdentity,
    next: &SyncGenerationIdentity,
) -> StoreResult<RegisteredSyncDevice> {
    validate_library_id(library_id)?;
    validate_device_id(device_id)?;
    validate_identity(expected_previous)?;
    validate_identity(next)?;
    require_device_and_common_base(connection, library_id, device_id, expected_previous)?;
    require_ack_proof(
        connection,
        library_id,
        device_id,
        expected_previous,
        expected_previous,
    )?;
    require_complete_generation(connection, library_id, next)?;
    let proof = VerifiedSharedAckLocalProof {
        library_id: library_id.to_owned(),
        shared_identity: next.clone(),
        local_identity: next.clone(),
        verified_at: now_millis()?,
    };
    advance_sync_device_shared_ack_in_transaction(
        connection,
        library_id,
        device_id,
        expected_previous,
        expected_previous,
        next,
        proof,
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn advance_sync_device_shared_ack_in_transaction(
    connection: &Connection,
    library_id: &str,
    device_id: &str,
    expected_previous_shared: &SyncGenerationIdentity,
    expected_previous_local: &SyncGenerationIdentity,
    next_shared: &SyncGenerationIdentity,
    next_proof: VerifiedSharedAckLocalProof,
) -> StoreResult<RegisteredSyncDevice> {
    validate_library_id(library_id)?;
    validate_device_id(device_id)?;
    validate_identity(expected_previous_shared)?;
    validate_identity(expected_previous_local)?;
    validate_identity(next_shared)?;
    if next_proof.library_id != library_id || next_proof.shared_identity != *next_shared {
        return validation(
            "verified acknowledgement proof does not match the requested shared ACK",
        );
    }
    validate_identity(&next_proof.local_identity)?;
    require_complete_generation(connection, library_id, &next_proof.local_identity)?;

    let current =
        load_device(connection, library_id, device_id)?.ok_or_else(|| StoreError::Validation {
            message: "sync device is not registered".to_owned(),
        })?;
    match current.status {
        RegisteredSyncDeviceStatus::Active => {}
        RegisteredSyncDeviceStatus::Revoked => {
            return sync_conflict("revoked sync device cannot acknowledge a generation")
        }
        RegisteredSyncDeviceStatus::Forgotten => {
            return sync_conflict("forgotten sync device cannot acknowledge a generation")
        }
    }

    let common = load_common_base(connection, library_id, device_id)?;
    let stored_proof = load_ack_proof(connection, library_id, device_id)?;
    if current.acknowledged_generation == *next_shared && common.as_ref() == Some(next_shared) {
        let current_proof = stored_proof
            .as_ref()
            .ok_or_else(|| StoreError::Validation {
                message: "sync device acknowledgement proof is missing".to_owned(),
            })?;
        require_ack_proof(
            connection,
            library_id,
            device_id,
            next_shared,
            &current_proof.local_identity,
        )?;
        if current_proof.local_identity == next_proof.local_identity {
            return Ok(current);
        }
        return sync_conflict("sync device acknowledgement proof differs at the same shared ACK");
    }
    if current.acknowledged_generation != *expected_previous_shared
        || common.as_ref() != Some(expected_previous_shared)
    {
        return sync_conflict("sync device acknowledgement does not match expected state");
    }
    require_ack_proof(
        connection,
        library_id,
        device_id,
        expected_previous_shared,
        expected_previous_local,
    )?;

    let sequence_order = compare_sequences(
        &next_shared.generation_sequence,
        &current.acknowledged_generation.generation_sequence,
    );
    if sequence_order == std::cmp::Ordering::Less {
        return sync_conflict("sync device acknowledgement cannot regress");
    }
    if sequence_order == std::cmp::Ordering::Equal
        && next_shared != &current.acknowledged_generation
    {
        return sync_conflict("sync device acknowledgement cannot fork at the same sequence");
    }
    if next_shared.generation_id == current.acknowledged_generation.generation_id
        && next_shared != &current.acknowledged_generation
    {
        return sync_conflict("sync device generation id cannot change identity");
    }
    if sequence_order == std::cmp::Ordering::Equal {
        return sync_conflict("sync device acknowledgement proof differs at the same shared ACK");
    }
    let acknowledged_at = current
        .acknowledged_at
        .max(current.registered_at)
        .max(now_millis()?);
    let common_updated = connection.execute(
        "UPDATE logical_peer_common_bases
         SET generation_id = ?1, manifest_hash = ?2,
             generation_sequence = ?3, updated_at = ?4
         WHERE peer_id = ?5 AND library_id = ?6
           AND generation_id = ?7 AND manifest_hash = ?8
           AND generation_sequence = ?9",
        params![
            next_shared.generation_id,
            next_shared.manifest_hash,
            next_shared.generation_sequence,
            acknowledged_at,
            device_id,
            library_id,
            expected_previous_shared.generation_id,
            expected_previous_shared.manifest_hash,
            expected_previous_shared.generation_sequence,
        ],
    )?;
    let device_updated = connection.execute(
        "UPDATE logical_sync_devices
         SET acknowledged_generation_id = ?1,
             acknowledged_manifest_hash = ?2,
             acknowledged_generation_sequence = ?3,
             acknowledged_at = ?4
         WHERE library_id = ?5 AND device_id = ?6 AND status = 'active'
           AND acknowledged_generation_id = ?7
           AND acknowledged_manifest_hash = ?8
           AND acknowledged_generation_sequence = ?9",
        params![
            next_shared.generation_id,
            next_shared.manifest_hash,
            next_shared.generation_sequence,
            acknowledged_at,
            library_id,
            device_id,
            expected_previous_shared.generation_id,
            expected_previous_shared.manifest_hash,
            expected_previous_shared.generation_sequence,
        ],
    )?;
    let proof_updated = connection.execute(
        "UPDATE logical_sync_device_ack_proofs
         SET shared_generation_id = ?1, shared_manifest_hash = ?2,
             shared_generation_sequence = ?3,
             local_generation_id = ?4, local_manifest_hash = ?5,
             local_generation_sequence = ?6, verified_at = ?7
         WHERE library_id = ?8 AND device_id = ?9
           AND shared_generation_id = ?10 AND shared_manifest_hash = ?11
           AND shared_generation_sequence = ?12
           AND local_generation_id = ?13 AND local_manifest_hash = ?14
           AND local_generation_sequence = ?15",
        params![
            next_proof.shared_identity.generation_id,
            next_proof.shared_identity.manifest_hash,
            next_proof.shared_identity.generation_sequence,
            next_proof.local_identity.generation_id,
            next_proof.local_identity.manifest_hash,
            next_proof.local_identity.generation_sequence,
            acknowledged_at.max(next_proof.verified_at),
            library_id,
            device_id,
            expected_previous_shared.generation_id,
            expected_previous_shared.manifest_hash,
            expected_previous_shared.generation_sequence,
            expected_previous_local.generation_id,
            expected_previous_local.manifest_hash,
            expected_previous_local.generation_sequence,
        ],
    )?;
    if common_updated != 1 || device_updated != 1 || proof_updated != 1 {
        return sync_conflict("sync device acknowledgement state changed concurrently");
    }
    load_device(connection, library_id, device_id)?.ok_or_else(|| StoreError::Store {
        message: "acknowledged sync device disappeared before commit".to_owned(),
    })
}

fn insert_exact_local_proof(
    connection: &Connection,
    library_id: &str,
    device_id: &str,
    identity: &SyncGenerationIdentity,
    verified_at: i64,
) -> StoreResult<()> {
    require_complete_generation(connection, library_id, identity)?;
    connection.execute(
        "INSERT INTO logical_sync_device_ack_proofs (
            library_id, device_id,
            shared_generation_id, shared_manifest_hash, shared_generation_sequence,
            local_generation_id, local_manifest_hash, local_generation_sequence,
            verified_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?3, ?4, ?5, ?6)",
        params![
            library_id,
            device_id,
            identity.generation_id,
            identity.manifest_hash,
            identity.generation_sequence,
            verified_at,
        ],
    )?;
    Ok(())
}

fn load_common_base(
    connection: &Connection,
    library_id: &str,
    device_id: &str,
) -> StoreResult<Option<SyncGenerationIdentity>> {
    connection
        .query_row(
            "SELECT generation_id, manifest_hash, generation_sequence
             FROM logical_peer_common_bases
             WHERE peer_id = ?1 AND library_id = ?2",
            params![device_id, library_id],
            |row| {
                Ok(SyncGenerationIdentity {
                    generation_id: row.get(0)?,
                    manifest_hash: row.get(1)?,
                    generation_sequence: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn load_ack_proof(
    connection: &Connection,
    library_id: &str,
    device_id: &str,
) -> StoreResult<Option<StoredSyncDeviceAckProof>> {
    connection
        .query_row(
            "SELECT shared_generation_id, shared_manifest_hash,
                    shared_generation_sequence, local_generation_id,
                    local_manifest_hash, local_generation_sequence, verified_at
             FROM logical_sync_device_ack_proofs
             WHERE library_id = ?1 AND device_id = ?2",
            params![library_id, device_id],
            |row| {
                Ok(StoredSyncDeviceAckProof {
                    shared_identity: SyncGenerationIdentity {
                        generation_id: row.get(0)?,
                        manifest_hash: row.get(1)?,
                        generation_sequence: row.get(2)?,
                    },
                    local_identity: SyncGenerationIdentity {
                        generation_id: row.get(3)?,
                        manifest_hash: row.get(4)?,
                        generation_sequence: row.get(5)?,
                    },
                    verified_at: row.get(6)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn require_ack_proof(
    connection: &Connection,
    library_id: &str,
    device_id: &str,
    expected_shared: &SyncGenerationIdentity,
    expected_local: &SyncGenerationIdentity,
) -> StoreResult<StoredSyncDeviceAckProof> {
    let proof = load_ack_proof(connection, library_id, device_id)?.ok_or_else(|| {
        StoreError::Validation {
            message: "sync device acknowledgement proof is missing".to_owned(),
        }
    })?;
    validate_identity(&proof.shared_identity)?;
    validate_identity(&proof.local_identity)?;
    if proof.verified_at < 0
        || proof.shared_identity != *expected_shared
        || proof.local_identity != *expected_local
    {
        return validation("sync device acknowledgement proof does not match expected state");
    }
    require_complete_generation(connection, library_id, &proof.local_identity)?;
    Ok(proof)
}

fn load_device(
    connection: &Connection,
    library_id: &str,
    device_id: &str,
) -> StoreResult<Option<RegisteredSyncDevice>> {
    connection
        .query_row(
            "SELECT library_id, device_id, status,
                    acknowledged_generation_id, acknowledged_manifest_hash,
                    acknowledged_generation_sequence,
                    registered_at, acknowledged_at, revoked_at, forgotten_at
             FROM logical_sync_devices
             WHERE library_id = ?1 AND device_id = ?2",
            params![library_id, device_id],
            decode_device_row,
        )
        .optional()?
        .map(validate_device_row)
        .transpose()
}

fn decode_device_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RegisteredSyncDevice> {
    let status: String = row.get(2)?;
    let status = RegisteredSyncDeviceStatus::parse(&status).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(RegisteredSyncDevice {
        library_id: row.get(0)?,
        device_id: row.get(1)?,
        status,
        acknowledged_generation: SyncGenerationIdentity {
            generation_id: row.get(3)?,
            manifest_hash: row.get(4)?,
            generation_sequence: row.get(5)?,
        },
        registered_at: row.get(6)?,
        acknowledged_at: row.get(7)?,
        revoked_at: row.get(8)?,
        forgotten_at: row.get(9)?,
    })
}

fn validate_device_row(device: RegisteredSyncDevice) -> StoreResult<RegisteredSyncDevice> {
    validate_library_id(&device.library_id)?;
    validate_device_id(&device.device_id)?;
    validate_identity(&device.acknowledged_generation)?;
    if device.registered_at < 0 || device.acknowledged_at < device.registered_at {
        return validation("sync device registry contains invalid timestamps");
    }
    if device
        .revoked_at
        .is_some_and(|timestamp| timestamp < device.registered_at)
        || device
            .forgotten_at
            .is_some_and(|timestamp| timestamp < device.registered_at)
    {
        return validation("sync device registry contains invalid lifecycle timestamps");
    }
    match device.status {
        RegisteredSyncDeviceStatus::Active
            if device.revoked_at.is_none() && device.forgotten_at.is_none() => {}
        RegisteredSyncDeviceStatus::Revoked
            if device.revoked_at.is_some() && device.forgotten_at.is_none() => {}
        RegisteredSyncDeviceStatus::Forgotten if device.forgotten_at.is_some() => {}
        _ => return validation("sync device registry status timestamps are inconsistent"),
    }
    Ok(device)
}

fn require_device_and_common_base(
    connection: &Connection,
    library_id: &str,
    device_id: &str,
    expected: &SyncGenerationIdentity,
) -> StoreResult<RegisteredSyncDevice> {
    let device =
        load_device(connection, library_id, device_id)?.ok_or_else(|| StoreError::Validation {
            message: "sync device is not registered".to_owned(),
        })?;
    if device.acknowledged_generation != *expected {
        return sync_conflict("sync device acknowledgement does not match expected state");
    }
    let common_base = connection
        .query_row(
            "SELECT generation_id, manifest_hash, generation_sequence
             FROM logical_peer_common_bases
             WHERE peer_id = ?1 AND library_id = ?2",
            params![device_id, library_id],
            |row| {
                Ok(SyncGenerationIdentity {
                    generation_id: row.get(0)?,
                    manifest_hash: row.get(1)?,
                    generation_sequence: row.get(2)?,
                })
            },
        )
        .optional()?;
    if common_base.as_ref() != Some(expected) {
        return sync_conflict("sync device common base does not match expected state");
    }
    Ok(device)
}

fn require_common_base_identity(
    connection: &Connection,
    library_id: &str,
    device_id: &str,
    expected: &SyncGenerationIdentity,
) -> StoreResult<()> {
    let common_base = connection
        .query_row(
            "SELECT generation_id, manifest_hash, generation_sequence
             FROM logical_peer_common_bases
             WHERE peer_id = ?1 AND library_id = ?2",
            params![device_id, library_id],
            |row| {
                Ok(SyncGenerationIdentity {
                    generation_id: row.get(0)?,
                    manifest_hash: row.get(1)?,
                    generation_sequence: row.get(2)?,
                })
            },
        )
        .optional()?;
    if common_base.as_ref() != Some(expected) {
        return sync_conflict("sync device common base does not match expected state");
    }
    Ok(())
}

fn require_complete_generation(
    connection: &Connection,
    library_id: &str,
    identity: &SyncGenerationIdentity,
) -> StoreResult<()> {
    let exact: bool = connection.query_row(
        "SELECT EXISTS(
            SELECT 1 FROM logical_sync_generations
            WHERE library_id = ?1 AND generation_id = ?2
              AND manifest_hash = ?3 AND generation_sequence = ?4
              AND state = 'complete' AND completed_at IS NOT NULL
         )",
        params![
            library_id,
            identity.generation_id,
            identity.manifest_hash,
            identity.generation_sequence,
        ],
        |row| row.get(0),
    )?;
    if !exact {
        return validation("sync acknowledgement must identify an exact complete generation");
    }
    Ok(())
}

fn require_active_head(
    connection: &Connection,
    expected: &SyncGenerationIdentity,
) -> StoreResult<()> {
    let actual = connection
        .query_row(
            "SELECT generation.generation_id, generation.manifest_hash,
                    generation.generation_sequence
             FROM logical_library_head AS head
             JOIN logical_sync_generations AS generation
               ON generation.library_id = head.library_id
              AND generation.generation_id = head.generation_id
             WHERE head.singleton = 1 AND generation.state = 'complete'",
            [],
            |row| {
                Ok(SyncGenerationIdentity {
                    generation_id: row.get(0)?,
                    manifest_hash: row.get(1)?,
                    generation_sequence: row.get(2)?,
                })
            },
        )
        .optional()?;
    if actual.as_ref() != Some(expected) {
        return sync_conflict("tombstone collection generation is stale");
    }
    Ok(())
}

fn active_library_id(connection: &Connection) -> StoreResult<String> {
    connection
        .query_row(
            "SELECT library_id FROM logical_library_head WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| StoreError::Validation {
            message: "logical library head is absent".to_owned(),
        })
}

fn validate_registry_consistency(
    connection: &Connection,
    generation: &SyncGenerationIdentity,
) -> StoreResult<()> {
    let library_id = active_library_id(connection)?;
    require_complete_generation(connection, &library_id, generation)?;
    let inconsistent_count: i64 = connection.query_row(
        "SELECT (
            SELECT COUNT(*)
            FROM logical_sync_devices AS device
            LEFT JOIN logical_peer_common_bases AS common_base
              ON common_base.library_id = device.library_id
             AND common_base.peer_id = device.device_id
            LEFT JOIN logical_sync_device_ack_proofs AS proof
              ON proof.library_id = device.library_id
             AND proof.device_id = device.device_id
            LEFT JOIN logical_sync_generations AS local_generation
              ON local_generation.library_id = proof.library_id
             AND local_generation.generation_id = proof.local_generation_id
             AND local_generation.manifest_hash = proof.local_manifest_hash
             AND local_generation.generation_sequence = proof.local_generation_sequence
             AND local_generation.state = 'complete'
             AND local_generation.completed_at IS NOT NULL
            WHERE device.library_id = ?1
              AND (
                (device.status != 'forgotten' AND (
                    common_base.peer_id IS NULL
                    OR common_base.generation_id != device.acknowledged_generation_id
                    OR common_base.manifest_hash != device.acknowledged_manifest_hash
                    OR common_base.generation_sequence != device.acknowledged_generation_sequence
                    OR proof.device_id IS NULL
                    OR proof.shared_generation_id != device.acknowledged_generation_id
                    OR proof.shared_manifest_hash != device.acknowledged_manifest_hash
                    OR proof.shared_generation_sequence != device.acknowledged_generation_sequence
                    OR local_generation.generation_id IS NULL
                ))
                OR (device.status = 'forgotten' AND (
                    common_base.peer_id IS NOT NULL OR proof.device_id IS NOT NULL
                ))
              )
        ) + (
            SELECT COUNT(*)
            FROM logical_peer_common_bases AS common_base
            LEFT JOIN logical_sync_devices AS device
              ON device.library_id = common_base.library_id
             AND device.device_id = common_base.peer_id
            WHERE common_base.library_id = ?1 AND device.device_id IS NULL
        ) + (
            SELECT COUNT(*)
            FROM logical_sync_device_ack_proofs AS proof
            LEFT JOIN logical_sync_devices AS device
              ON device.library_id = proof.library_id
             AND device.device_id = proof.device_id
            WHERE proof.library_id = ?1
              AND (device.device_id IS NULL OR device.status = 'forgotten')
        )",
        [library_id.as_str()],
        |row| row.get(0),
    )?;
    if inconsistent_count != 0 {
        return validation("sync device registry and common bases are inconsistent");
    }
    Ok(())
}

fn list_plan_devices(
    connection: &Connection,
    generation: &SyncGenerationIdentity,
) -> StoreResult<Vec<TombstonePlanDevice>> {
    let library_id = active_library_id(connection)?;
    let mut statement = connection.prepare(
        "SELECT library_id, device_id, status,
                acknowledged_generation_id, acknowledged_manifest_hash,
                acknowledged_generation_sequence,
                registered_at, acknowledged_at, revoked_at, forgotten_at
         FROM logical_sync_devices
         WHERE library_id = ?1
         ORDER BY device_id",
    )?;
    let all_devices = statement
        .query_map([library_id], decode_device_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(validate_device_row)
        .collect::<StoreResult<Vec<_>>>()?;
    let devices = all_devices
        .into_iter()
        .filter(|device| device.status != RegisteredSyncDeviceStatus::Forgotten)
        .map(|device| {
            let proof = load_ack_proof(connection, &device.library_id, &device.device_id)?
                .ok_or_else(|| StoreError::Validation {
                    message: "sync device acknowledgement proof is missing".to_owned(),
                })?;
            require_ack_proof(
                connection,
                &device.library_id,
                &device.device_id,
                &device.acknowledged_generation,
                &proof.local_identity,
            )?;
            if compare_sequences(
                &proof.local_identity.generation_sequence,
                &generation.generation_sequence,
            ) == std::cmp::Ordering::Greater
            {
                return validation("sync device acknowledgement exceeds the active generation");
            }
            Ok(TombstonePlanDevice {
                device_id: device.device_id,
                status: device.status,
                local_generation_sequence: proof.local_identity.generation_sequence,
            })
        })
        .collect::<StoreResult<Vec<_>>>()?;
    Ok(devices)
}

fn encode_cursor(cursor: &TombstoneCursor) -> StoreResult<String> {
    Ok(hex::encode(serde_json::to_vec(cursor)?))
}

fn decode_cursor(value: &str) -> StoreResult<TombstoneCursor> {
    if value.len() > MAX_CURSOR_BYTES * 2 || value.len() % 2 != 0 {
        return validation("tombstone collection cursor is invalid");
    }
    let bytes = hex::decode(value).map_err(|_| StoreError::Validation {
        message: "tombstone collection cursor is invalid".to_owned(),
    })?;
    if bytes.len() > MAX_CURSOR_BYTES {
        return validation("tombstone collection cursor is invalid");
    }
    let cursor: TombstoneCursor = serde_json::from_slice(&bytes)?;
    validate_identity(&cursor.generation)?;
    if cursor.last_record_key.is_empty() || cursor.last_record_key.len() > 65_536 {
        return validation("tombstone collection cursor record key is invalid");
    }
    Ok(cursor)
}

fn validate_library_id(value: &str) -> StoreResult<()> {
    if value.is_empty() {
        return validation("sync library id must be nonempty");
    }
    Ok(())
}

fn validate_device_id(value: &str) -> StoreResult<()> {
    if value.is_empty() || value.chars().count() > MAX_DEVICE_ID_CHARS {
        return validation("sync device id must contain between 1 and 1024 characters");
    }
    Ok(())
}

fn validate_identity(identity: &SyncGenerationIdentity) -> StoreResult<()> {
    if identity.generation_id.is_empty() {
        return validation("sync generation id must be nonempty");
    }
    if identity.manifest_hash.len() != 64
        || !identity
            .manifest_hash
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return validation("sync manifest hash must be a lowercase SHA-256");
    }
    validate_sequence(&identity.generation_sequence)
}

fn validate_sequence(value: &str) -> StoreResult<()> {
    if value.is_empty()
        || value.len() > MAX_GENERATION_SEQUENCE_DIGITS
        || !value.bytes().all(|byte| byte.is_ascii_digit())
        || (value.len() > 1 && value.starts_with('0'))
    {
        return validation("sync generation sequence must be a canonical unsigned decimal string");
    }
    Ok(())
}

fn compare_sequences(left: &str, right: &str) -> std::cmp::Ordering {
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

fn now_millis() -> StoreResult<i64> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| StoreError::Store {
            message: "system clock predates the Unix epoch".to_owned(),
        })?
        .as_millis();
    i64::try_from(millis).map_err(|_| StoreError::Store {
        message: "system time exceeds the persistent timestamp range".to_owned(),
    })
}

fn validation<T>(message: &str) -> StoreResult<T> {
    Err(StoreError::Validation {
        message: message.to_owned(),
    })
}

fn sync_conflict<T>(message: &str) -> StoreResult<T> {
    Err(StoreError::Validation {
        message: message.to_owned(),
    })
}
