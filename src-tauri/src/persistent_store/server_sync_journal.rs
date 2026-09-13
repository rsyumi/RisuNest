use super::{server_sync_outbox::ServerDirtyKey, PersistentStore};
use crate::server_sync::{client::ServerConfig, credentials::StoredConfig, Result, SyncError};
use risunest_sync_wire::{
    canonical, operation_id, CommitIntent, Receipt, RecordVersion, RemoteHead, Sequence,
    TerminalStatus,
};
use rusqlite::{params, OptionalExtension, TransactionBehavior};
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReplicaStatus {
    pub local_revision: i64,
    pub reconciling: bool,
    pub configured: bool,
    pub endpoint: Option<String>,
    pub library_id: Option<String>,
    pub device_id: Option<String>,
    pub head: Option<RemoteHead>,
    pub dirty_records: i64,
    pub full_scan: bool,
    pub registration_required: bool,
    pub operation_pending: bool,
}
pub(crate) struct PendingOperation {
    pub intent: CommitIntent,
    pub phase: String,
    pub local_revision: i64,
}

impl PersistentStore {
    pub(crate) fn server_cache_references(
        &self,
        cache: &crate::server_sync::cache::Cache,
    ) -> Result<std::collections::BTreeSet<String>> {
        let mut hashes = std::collections::BTreeSet::new();
        for table in [
            "server_sync_base",
            "server_sync_remote",
            "server_sync_operation_records",
        ] {
            let mut statement = self
                .connection
                .prepare(&format!("SELECT version FROM {table}"))?;
            for version in statement.query_map([], |row| row.get::<_, String>(0))? {
                let version: RecordVersion = serde_json::from_str(&version?)
                    .map_err(|_| SyncError::new("invalid-local-server-version", 409))?;
                hashes.extend(cache.closure(&version)?);
            }
        }
        let mut statement = self
            .connection
            .prepare("SELECT hash FROM server_sync_objects")?;
        for hash in statement.query_map([], |row| row.get::<_, String>(0))? {
            hashes.insert(hash?);
        }
        Ok(hashes)
    }

    pub(crate) fn server_config(&self) -> Result<Option<ServerConfig>> {
        self.server_stored_config()?
            .map(|stored| stored.resolve(self.repository_root()))
            .transpose()
    }
    pub(crate) fn server_stored_config(&self) -> Result<Option<StoredConfig>> {
        let text: Option<String> = self
            .connection
            .query_row(
                "SELECT config FROM server_sync_state WHERE singleton=1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        text.map(|s| {
            serde_json::from_str(&s).map_err(|_| SyncError::new("invalid-local-server-config", 409))
        })
        .transpose()
    }
    /// Update only the address after an authenticated identity probe. Replica state is untouched.
    pub(crate) fn server_cache_endpoint(
        &mut self,
        previous: &ServerConfig,
        verified: &ServerConfig,
    ) -> Result<()> {
        if previous.endpoint == verified.endpoint {
            return Ok(());
        }
        verified.validate()?;
        if previous.library_id != verified.library_id
            || previous.device_id != verified.device_id
            || previous.token != verified.token
        {
            return Err(SyncError::new("device-identity-mismatch", 409));
        }
        let mut stored = self
            .server_stored_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        let before = serde_json::to_string(&stored)
            .map_err(|_| SyncError::new("invalid-local-server-config", 409))?;
        if stored.endpoint != previous.endpoint
            || stored.library_id != previous.library_id
            || stored.device_id != previous.device_id
        {
            return Err(SyncError::new("server-config-changed", 409));
        }
        stored.endpoint = verified.endpoint.clone();
        let after = serde_json::to_string(&stored)
            .map_err(|_| SyncError::new("invalid-local-server-config", 409))?;
        if self.connection.execute(
            "UPDATE server_sync_state SET config=?1 WHERE singleton=1 AND config=?2",
            params![after, before],
        )? != 1
        {
            return Err(SyncError::new("server-config-changed", 409));
        }
        if self.repository_root.join("asset-residency.sqlite").exists() {
            crate::server_sync::residency::Residency::open(&self.repository_root)?
                .replace_access_config(&stored)?;
        }
        Ok(())
    }
    pub(crate) fn server_bind(&mut self, config: &ServerConfig) -> Result<()> {
        config.validate()?;
        let root = self.repository_root().to_owned();
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.query_row::<bool, _, _>(
            "SELECT EXISTS(SELECT 1 FROM server_sync_state)",
            [],
            |r| r.get(0),
        )? {
            return Err(SyncError::new("server-already-bound", 409));
        }
        let stored = StoredConfig::persist(&root, config)?;
        let outcome = (|| -> Result<()> {
            tx.execute(
                "INSERT INTO server_sync_state(singleton,config) VALUES(1,?1)",
                [serde_json::to_string(&stored)
                    .map_err(|_| SyncError::new("invalid-server-config", 400))?],
            )?;
            tx.commit()?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = stored.remove(&root);
        }
        outcome
    }
    /// Disconnect keeps PDS and cached immutable bytes. An unresolved operation
    /// must first be reconciled with its receipt so it cannot be forgotten.
    pub(crate) fn server_unbind(&mut self) -> Result<()> {
        if self.asset_residency_status()?.has_remote_or_missing() {
            return Err(SyncError::new("download-all-assets-before-unbind", 409));
        }
        let stored = self.server_stored_config()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if tx.query_row::<bool, _, _>(
            "SELECT EXISTS(SELECT 1 FROM server_sync_operation)",
            [],
            |r| r.get(0),
        )? {
            return Err(SyncError::new("resolve-pending-operation-first", 409));
        }
        for table in [
            "server_sync_clear_members",
            "server_sync_clears",
            "server_sync_dirty",
            "server_sync_base",
            "server_sync_scope_base",
            "server_sync_scope_clear_base",
            "server_sync_remote",
            "server_sync_remote_dirty",
            "server_sync_remote_cursor",
            "server_sync_operation_records",
            "server_sync_operation_pages",
            "server_sync_operation_scopes",
            "server_sync_state",
        ] {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        tx.commit()?;
        if let Some(stored) = stored {
            let _ = stored.remove(self.repository_root());
        }
        Ok(())
    }
    pub(crate) fn server_status(&self) -> Result<ReplicaStatus> {
        let config = self.server_stored_config()?;
        let (head,full_scan,registration_required)=self.connection.query_row("SELECT head,full_scan,registration_required FROM server_sync_state WHERE singleton=1",[],|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,bool>(1)?,r.get::<_,bool>(2)?))).optional()?.unwrap_or((None,false,false));
        Ok(ReplicaStatus {
            local_revision: self.revision()?,
            reconciling: self
                .connection
                .query_row(
                    "SELECT reconciling FROM server_sync_state WHERE singleton=1",
                    [],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(false),
            configured: config.is_some(),
            endpoint: config.as_ref().map(|c| c.endpoint.clone()),
            library_id: config.as_ref().map(|c| c.library_id.clone()),
            device_id: config.as_ref().map(|c| c.device_id.clone()),
            head: head
                .map(|s| {
                    serde_json::from_str(&s)
                        .map_err(|_| SyncError::new("invalid-local-cursor", 409))
                })
                .transpose()?,
            dirty_records: self.connection.query_row(
                "SELECT count(*) FROM server_sync_dirty",
                [],
                |r| r.get(0),
            )?,
            full_scan,
            registration_required,
            operation_pending: self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM server_sync_operation)",
                [],
                |r| r.get(0),
            )?,
        })
    }
    /// The native command has verified a fresh credential and revoked old device.
    /// Keep the old bases and local edits for an explicit three-way reconciliation.
    pub(crate) fn server_replace_registration(
        &mut self,
        config: &ServerConfig,
        expected_revision: i64,
    ) -> Result<()> {
        config.validate()?;
        let previous = self
            .server_stored_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        if previous.library_id != config.library_id || previous.device_id == config.device_id {
            return Err(SyncError::new("new-device-registration-required", 409));
        }
        self.server_reset_reconciliation(expected_revision, Some(config))
    }
    pub(crate) fn server_reconcile_epoch(
        &mut self,
        head: &RemoteHead,
        expected_revision: i64,
    ) -> Result<()> {
        head.validate()?;
        let status = self.server_status()?;
        if status.library_id.as_ref() != Some(&head.library_id)
            || !status
                .head
                .as_ref()
                .is_some_and(|old| old.epoch != head.epoch)
        {
            return Err(SyncError::new("epoch-reconciliation-not-required", 409));
        }
        if status.registration_required {
            return Err(SyncError::new("device-registration-required", 409));
        }
        self.server_reset_reconciliation(expected_revision, None)
    }
    fn server_reset_reconciliation(
        &mut self,
        expected_revision: i64,
        config: Option<&ServerConfig>,
    ) -> Result<()> {
        if self.revision()? != expected_revision {
            return Err(SyncError::new("local-revision-changed", 409));
        }
        self.snapshot_create("server-sync-recovery")?;
        let root = self.repository_root().to_owned();
        let previous = self.server_stored_config()?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if super::current_revision(&tx)? != expected_revision {
            return Err(SyncError::new("local-revision-changed", 409));
        }
        for table in [
            "server_sync_remote",
            "server_sync_remote_dirty",
            "server_sync_remote_cursor",
            "server_sync_operation",
            "server_sync_operation_records",
            "server_sync_operation_pages",
            "server_sync_operation_scopes",
        ] {
            tx.execute(&format!("DELETE FROM {table}"), [])?;
        }
        tx.execute(
            "UPDATE server_sync_state SET head=NULL,full_scan=1,reconciling=1",
            [],
        )?;
        let replacement = config
            .map(|config| StoredConfig::persist(&root, config))
            .transpose()?;
        let outcome = (|| -> Result<()> {
            if let Some(config) = &replacement {
                tx.execute(
                "UPDATE server_sync_state SET config=?1,next_sequence='1',registration_required=0",
                [serde_json::to_string(config)
                    .map_err(|_| SyncError::new("invalid-server-config", 400))?],
            )?;
            }
            tx.commit()?;
            Ok(())
        })();
        if outcome.is_ok() && replacement.is_some() {
            crate::server_sync::residency::Residency::open(&root)?
                .replace_access_config(replacement.as_ref().unwrap())?;
            if let Some(previous) = previous {
                let _ = previous.remove(&root);
            }
        } else if let Some(replacement) = replacement {
            let _ = replacement.remove(&root);
        }
        outcome
    }
    pub(crate) fn server_pending(&self) -> Result<Option<PendingOperation>> {
        let row = self
            .connection
            .query_row(
                "SELECT intent,phase,local_revision FROM server_sync_operation WHERE singleton=1",
                [],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(intent, phase, local_revision)| {
            Ok(PendingOperation {
                intent: canonical::decode(
                    intent.as_bytes(),
                    risunest_sync_wire::MAX_METADATA_BYTES,
                )?,
                phase,
                local_revision,
            })
        })
        .transpose()
    }
    /// Reserve the sequence and exact intent before any commit HTTP request.
    /// Stage IDs may later change; the logical digest and sequence cannot.
    pub(crate) fn server_reserve(
        &mut self,
        head: &RemoteHead,
        digest: String,
        stage_id: String,
        local_revision: i64,
    ) -> Result<CommitIntent> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (next, registration): (String, bool) = tx.query_row(
            "SELECT next_sequence,registration_required FROM server_sync_state WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        if registration {
            return Err(SyncError::new("device-registration-required", 409));
        }
        let sequence: Sequence = next.try_into()?;
        let intent = CommitIntent {
            device_operation_seq: sequence.clone(),
            expected_head: head.clone(),
            changes_digest: digest,
            staged_changes_id: stage_id,
        };
        intent.validate()?;
        tx.execute("INSERT INTO server_sync_operation(singleton,sequence,intent,phase,local_revision) VALUES(1,?1,?2,'prepared',?3)",params![sequence.as_str(),String::from_utf8(canonical::encode(&intent)?).map_err(|_|SyncError::new("invalid-intent",400))?,local_revision])?;
        tx.execute(
            "UPDATE server_sync_state SET next_sequence=?1 WHERE singleton=1",
            [sequence.next()?.as_str()],
        )?;
        tx.commit()?;
        Ok(intent)
    }
    pub(crate) fn server_restage(&mut self, stage_id: String) -> Result<CommitIntent> {
        let mut pending = self
            .server_pending()?
            .ok_or_else(|| SyncError::new("missing-operation", 409))?;
        pending.intent.staged_changes_id = stage_id;
        pending.intent.validate()?;
        self.connection.execute(
            "UPDATE server_sync_operation SET intent=?1 WHERE singleton=1",
            [String::from_utf8(canonical::encode(&pending.intent)?)
                .map_err(|_| SyncError::new("invalid-intent", 400))?],
        )?;
        Ok(pending.intent)
    }
    /// Receipt evidence is checked before updating any local durable identity.
    /// A committed receipt is retained until the corresponding remote revision
    /// has been semantically applied, including changes from concurrent devices.
    pub(crate) fn server_observe_receipt(&mut self, receipt: &Receipt) -> Result<bool> {
        let pending = self
            .server_pending()?
            .ok_or_else(|| SyncError::new("missing-operation", 409))?;
        let config = self
            .server_config()?
            .ok_or_else(|| SyncError::new("server-not-bound", 409))?;
        receipt.head.validate()?;
        if receipt.operation_id
            != operation_id(
                &config.library_id,
                &config.device_id,
                &pending.intent.device_operation_seq,
            )?
            || receipt.device_operation_seq != pending.intent.device_operation_seq
            || receipt.intent_digest != pending.intent.digest()?
            || receipt.head.library_id != config.library_id
            || receipt.head.epoch != pending.intent.expected_head.epoch
        {
            return Err(SyncError::new("receipt-identity-mismatch", 409));
        }
        let committed = receipt.status == TerminalStatus::Committed;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if committed {
            if receipt.head.seq <= pending.intent.expected_head.seq {
                return Err(SyncError::new("invalid-committed-receipt", 409));
            }
            tx.execute(
                "UPDATE server_sync_operation SET phase=?1 WHERE singleton=1",
                [String::from_utf8(canonical::encode(receipt)?)
                    .map_err(|_| SyncError::new("invalid-receipt", 409))?],
            )?;
        } else {
            // Stale/failed is terminal for this sequence, while every local dirty
            // record and clear intent remains available for a new proposal.
            for table in [
                "server_sync_operation",
                "server_sync_operation_records",
                "server_sync_operation_pages",
                "server_sync_operation_scopes",
            ] {
                tx.execute(&format!("DELETE FROM {table}"), [])?;
            }
        }
        tx.commit()?;
        Ok(committed)
    }
    pub(crate) fn server_base(&self, key: &str) -> Result<(RecordVersion, Option<String>)> {
        let row = self
            .connection
            .query_row(
                "SELECT version,local_hash FROM server_sync_base WHERE key=?1",
                [key],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        match row {
            Some((version, hash)) => Ok((
                canonical::decode(version.as_bytes(), risunest_sync_wire::MAX_METADATA_BYTES)?,
                hash,
            )),
            None => Ok((RecordVersion::Absent, None)),
        }
    }
    pub(crate) fn server_record_prepared(
        &self,
        key: &str,
        version: &RecordVersion,
        local_hash: Option<&str>,
        dirty: &ServerDirtyKey,
    ) -> Result<()> {
        self.connection.execute("INSERT INTO server_sync_operation_records VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(key) DO UPDATE SET version=excluded.version,local_hash=excluded.local_hash,revision=excluded.revision",params![key,String::from_utf8(canonical::encode(version)?).map_err(|_|SyncError::new("invalid-version",400))?,local_hash,dirty.kind,dirty.key1,dirty.key2,dirty.revision])?;
        Ok(())
    }
}
