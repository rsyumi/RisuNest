//! Device-local connection settings. Only opaque OS-vault references are stored
//! here; sync bases and job outcomes remain authoritative in the library PDS.
use super::{capabilities::Capabilities, contract::*, packaging::RemoteObject};
use risunest_external_storage_format::format::Descriptor;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct StoredConnection {
    pub id: String,
    pub config: ConnectionConfig,
    pub descriptor: Descriptor,
    pub descriptor_locator: RemoteLocator,
    pub provider_repository_id: String,
    pub credential_ref: String,
    pub root_key_ref: String,
    pub capabilities: Capabilities,
    pub created_at_ms: u64,
    pub last_sync_at_ms: Option<u64>,
    pub last_backup_at_ms: Option<u64>,
    /// Backup connections only. Changing it applies to work started
    /// afterwards and never rewrites an existing point.
    #[serde(default)]
    pub capture_policy: Option<super::connection::CapturePolicy>,
    /// All connections. `None` means `RetentionPolicy::DEFAULT`; it is the
    /// default for a connection that never set one.
    #[serde(default)]
    pub retention_policy: Option<super::connection::RetentionPolicy>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PendingStoredConnection {
    pub id: String,
    pub config: ConnectionConfig,
    /// Fixed before any remote mutation. The descriptor is selected exactly
    /// once after authenticated capabilities are known.
    pub repository_id: String,
    pub descriptor: Option<Descriptor>,
    pub create: bool,
    #[serde(default)]
    pub capture_policy: Option<super::connection::CapturePolicy>,
    pub provider_repository_id: Option<String>,
    pub credential_ref: String,
    pub root_key_ref: String,
    pub created_at_ms: u64,
}
#[derive(Clone, Copy)]
pub(crate) enum CompletionKind {
    Sync,
    Backup,
}

pub(crate) struct ConnectionStore(Connection);
fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn storage(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

impl ConnectionStore {
    pub(crate) fn open(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root).map_err(storage)?;
        if crate::trust_boundary::is_link_like(&std::fs::symlink_metadata(root).map_err(storage)?) {
            return Err(corrupt());
        }
        let path = root.join("external-connections.sqlite");
        if path.exists() {
            crate::trust_boundary::open_regular_source(&path).map_err(storage)?;
        }
        let db = Connection::open(path).map_err(storage)?;
        db.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(storage)?;
        db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            CREATE TABLE IF NOT EXISTS connections(id TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS pending_connections(id TEXT PRIMARY KEY,value TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS discovery(connection_id TEXT NOT NULL,id TEXT NOT NULL,value TEXT NOT NULL,PRIMARY KEY(connection_id,id));").map_err(storage)?;
        Ok(Self(db))
    }
    pub fn list(&self) -> Result<Vec<StoredConnection>> {
        let mut query = self
            .0
            .prepare("SELECT value FROM connections ORDER BY id")
            .map_err(storage)?;
        let rows = query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(storage)?;
        let mut result = Vec::new();
        for row in rows {
            result.push(decode(&row.map_err(storage)?)?);
        }
        Ok(result)
    }
    pub fn read(&self, id: &str) -> Result<StoredConnection> {
        let encoded: Option<String> = self
            .0
            .query_row("SELECT value FROM connections WHERE id=?1", [id], |row| {
                row.get(0)
            })
            .optional()
            .map_err(storage)?;
        let result = decode(&encoded.ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?)?;
        if result.id != id {
            return Err(corrupt());
        }
        Ok(result)
    }
    /// The connection that already holds this remote repository, if there is
    /// one. Retention settings and the one-job-per-connection rule are per
    /// connection while the repository is not, so a repository two connections
    /// already hold is a state this build cannot produce and is reported.
    pub fn identity_holder(&self, identity: &str) -> Result<Option<String>> {
        let mut held: BTreeMap<String, String> = BTreeMap::new();
        for connection in self.list()? {
            if held
                .insert(
                    connection.descriptor_locator.connection_identity,
                    connection.id,
                )
                .is_some()
            {
                return Err(corrupt());
            }
        }
        Ok(held.remove(identity))
    }
    fn require_unheld_identity(&self, identity: &str) -> Result<()> {
        match self.identity_holder(identity)? {
            Some(_) => Err(ProviderError::new(ErrorKind::PreconditionFailed)),
            None => Ok(()),
        }
    }
    pub fn insert(&mut self, connection: &StoredConnection) -> Result<()> {
        connection.descriptor.validate().map_err(|_| corrupt())?;
        self.require_unheld_identity(&connection.descriptor_locator.connection_identity)?;
        let encoded = serde_json::to_string(connection).map_err(storage)?;
        decode(&encoded)?;
        self.0
            .execute(
                "INSERT INTO connections VALUES(?1,?2)",
                params![connection.id, encoded],
            )
            .map_err(storage)?;
        Ok(())
    }
    /// Replaces a backup connection's capture policy. Work already started
    /// keeps the policy it fixed, and no existing point is rewritten.
    pub fn set_capture_policy(
        &mut self,
        id: &str,
        policy: super::connection::CapturePolicy,
    ) -> Result<StoredConnection> {
        let mut connection = self.read(id)?;
        if connection.capture_policy.is_none() {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        connection.capture_policy = Some(policy);
        self.write(id, connection)
    }
    /// Replaces how long a connection keeps the automatic backup points this
    /// device made. A cleanup already running keeps the policy it fixed.
    pub fn set_retention_policy(
        &mut self,
        id: &str,
        policy: super::connection::RetentionPolicy,
    ) -> Result<StoredConnection> {
        policy.validate()?;
        let mut connection = self.read(id)?;
        connection.retention_policy = Some(policy);
        self.write(id, connection)
    }
    fn write(&mut self, id: &str, connection: StoredConnection) -> Result<StoredConnection> {
        let encoded = serde_json::to_string(&connection).map_err(storage)?;
        decode(&encoded)?;
        self.0
            .execute(
                "UPDATE connections SET value=?2 WHERE id=?1",
                params![id, encoded],
            )
            .map_err(storage)?;
        Ok(connection)
    }
    pub fn put_pending(&mut self, connection: &PendingStoredConnection) -> Result<()> {
        validate_pending(connection)?;
        let encoded = serde_json::to_string(connection).map_err(storage)?;
        let tx = self.0.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
        let previous: Option<String> = tx.query_row(
            "SELECT value FROM pending_connections WHERE id=?1", [&connection.id],
            |row| row.get(0),
        ).optional().map_err(storage)?;
        if let Some(previous) = previous {
            let pinned = decode_pending(&previous)?;
            let immutable_changed = pinned.id != connection.id
                || pinned.config != connection.config
                || pinned.repository_id != connection.repository_id
                || pinned.create != connection.create
                || pinned.root_key_ref != connection.root_key_ref
                || pinned.capture_policy != connection.capture_policy
                || pinned.created_at_ms != connection.created_at_ms;
            let descriptor_changed = match (&pinned.descriptor, &connection.descriptor) {
                (Some(previous), Some(proposed)) => previous != proposed,
                (Some(_), None) => true,
                (None, _) => false,
            };
            let provider_changed = match (
                &pinned.provider_repository_id,
                &connection.provider_repository_id,
            ) {
                (Some(previous), Some(proposed)) => previous != proposed,
                (Some(_), None) => true,
                (None, _) => false,
            };
            if immutable_changed || descriptor_changed || provider_changed {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
        }
        tx.execute(
            "INSERT INTO pending_connections VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET value=excluded.value",
            params![connection.id, encoded],
        ).map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(())
    }
    pub fn pending(&self, id: &str) -> Result<PendingStoredConnection> {
        let encoded: Option<String> = self
            .0
            .query_row(
                "SELECT value FROM pending_connections WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        let value: PendingStoredConnection =
            decode_pending(&encoded.ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?)?;
        if value.id != id {
            return Err(corrupt());
        }
        Ok(value)
    }
    pub fn pending_create_for(
        &self,
        config: &ConnectionConfig,
        capture_policy: Option<super::connection::CapturePolicy>,
    ) -> Result<Option<PendingStoredConnection>> {
        let mut query = self
            .0
            .prepare("SELECT value FROM pending_connections ORDER BY id")
            .map_err(storage)?;
        let rows = query
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(storage)?;
        let mut matched = None;
        for row in rows {
            let pending = decode_pending(&row.map_err(storage)?)?;
            if pending.create
                && pending.config == *config
                && pending.capture_policy == capture_policy
            {
                if matched.is_some() {
                    return Err(corrupt());
                }
                matched = Some(pending);
            }
        }
        Ok(matched)
    }
    pub fn promote_pending(
        &mut self,
        id: &str,
        descriptor_locator: RemoteLocator,
        capabilities: Capabilities,
    ) -> Result<StoredConnection> {
        let pending = self.pending(id)?;
        let provider_repository_id = pending.provider_repository_id.ok_or_else(corrupt)?;
        let descriptor = pending.descriptor.ok_or_else(corrupt)?;
        let connection = StoredConnection {
            id: pending.id,
            config: pending.config,
            descriptor,
            descriptor_locator,
            provider_repository_id,
            credential_ref: pending.credential_ref,
            capture_policy: pending.capture_policy,
            retention_policy: None,
            root_key_ref: pending.root_key_ref,
            capabilities,
            created_at_ms: pending.created_at_ms,
            last_sync_at_ms: None,
            last_backup_at_ms: None,
        };
        connection.descriptor.validate().map_err(|_| corrupt())?;
        self.require_unheld_identity(&connection.descriptor_locator.connection_identity)?;
        let encoded = serde_json::to_string(&connection).map_err(storage)?;
        decode(&encoded)?;
        let tx = self.0.transaction().map_err(storage)?;
        tx.execute(
            "INSERT INTO connections VALUES(?1,?2)",
            params![&connection.id, encoded],
        )
        .map_err(storage)?;
        tx.execute(
            "DELETE FROM pending_connections WHERE id=?1",
            [&connection.id],
        )
        .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(connection)
    }
    pub fn remove_pending(&self, id: &str) -> Result<()> {
        self.0
            .execute("DELETE FROM pending_connections WHERE id=?1", [id])
            .map_err(storage)?;
        Ok(())
    }
    /// Caller has already cancelled/settled jobs and changed the PDS selection.
    pub fn remove(&mut self, id: &str) -> Result<StoredConnection> {
        let connection = self.read(id)?;
        let tx = self.0.transaction().map_err(storage)?;
        tx.execute("DELETE FROM discovery WHERE connection_id=?1", [id])
            .map_err(storage)?;
        tx.execute("DELETE FROM connections WHERE id=?1", [id])
            .map_err(storage)?;
        tx.commit().map_err(storage)?;
        Ok(connection)
    }
    /// Records only a completed operation; starting or preparing work never calls this.
    pub fn record_completion(
        &mut self,
        id: &str,
        kind: CompletionKind,
        completed_at_ms: u64,
    ) -> Result<()> {
        if completed_at_ms == 0 {
            return Err(corrupt());
        }
        let tx = self.0.transaction_with_behavior(TransactionBehavior::Immediate).map_err(storage)?;
        let encoded: Option<String> = tx.query_row(
            "SELECT value FROM connections WHERE id=?1", [id], |row| row.get(0),
        ).optional().map_err(storage)?;
        let mut connection = decode(&encoded.ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?)?;
        if connection.id != id {
            return Err(corrupt());
        }
        let timestamp = match kind {
            CompletionKind::Sync if connection.descriptor.publication_strategy.is_some() => {
                &mut connection.last_sync_at_ms
            }
            CompletionKind::Sync => return Err(ProviderError::new(ErrorKind::Unsupported)),
            CompletionKind::Backup => &mut connection.last_backup_at_ms,
        };
        *timestamp = Some(timestamp.unwrap_or(0).max(completed_at_ms));
        let encoded = serde_json::to_string(&connection).map_err(storage)?;
        tx.execute("UPDATE connections SET value=?2 WHERE id=?1", params![id, encoded]).map_err(storage)?;
        tx.commit().map_err(storage)
    }

    pub fn remember_discovery(
        &self,
        connection: &str,
        id: &str,
        value: &RemoteObject,
    ) -> Result<()> {
        validate_discovery(&self.read(connection)?, id, value)?;
        let encoded = serde_json::to_string(value).map_err(storage)?;
        if encoded.len() > 256 * 1024 {
            return Err(corrupt());
        }
        self.0.execute("INSERT INTO discovery VALUES(?1,?2,?3) ON CONFLICT(connection_id,id) DO UPDATE SET value=excluded.value",
            params![connection, id, encoded]).map_err(storage)?;
        Ok(())
    }
    pub fn discovery(
        &self,
        connection: &str,
        id: &str,
        role: ObjectRole,
        expected_ciphertext_sha256: Option<&str>,
    ) -> Result<RemoteObject> {
        let value = self.discovery_snapshot(connection, id)?;
        if value.role != role || expected_ciphertext_sha256.is_some_and(|hash| {
            !crate::trust_boundary::is_lower_hex_256(hash) || hash != value.ciphertext_sha256
        }) {
            return Err(corrupt());
        }
        Ok(value)
    }

    pub(crate) fn discovery_snapshot(
        &self,
        connection: &str,
        id: &str,
    ) -> Result<RemoteObject> {
        let current = self.read(connection)?;
        let encoded: Option<String> = self
            .0
            .query_row(
                "SELECT value FROM discovery WHERE connection_id=?1 AND id=?2",
                params![connection, id],
                |row| row.get(0),
            )
            .optional()
            .map_err(storage)?;
        let encoded = encoded.ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        if encoded.len() > 256 * 1024 {
            return Err(corrupt());
        }
        let value: RemoteObject = serde_json::from_str(&encoded).map_err(|_| corrupt())?;
        validate_discovery(&current, id, &value)?;
        Ok(value)
    }

    pub(crate) fn forget_discovery(&self, connection: &str, id: &str) -> Result<()> {
        self.0
            .execute(
                "DELETE FROM discovery WHERE connection_id=?1 AND id=?2",
                params![connection, id],
            )
            .map_err(storage)?;
        Ok(())
    }
}

fn validate_discovery(connection: &StoredConnection, id: &str, value: &RemoteObject) -> Result<()> {
    let locator = &value.receipt.locator;
    if id.is_empty() || id.len() > 256 || id.chars().any(char::is_control)
        || value.object_id != format!("snapshot-{id}")
        || value.repository_id != connection.descriptor.repository_id
        || !matches!(value.role, ObjectRole::SyncState | ObjectRole::BackupBundle)
        || locator.connection_identity != connection.descriptor_locator.connection_identity
        || locator.object.is_empty() || locator.object.len() > 8192 || locator.object.contains('\0')
        || !value.receipt.complete || value.receipt.byte_length == 0 || value.plaintext_length == 0
        || !crate::trust_boundary::is_lower_hex_256(&value.ciphertext_sha256)
        || !crate::trust_boundary::is_lower_hex_256(&value.plaintext_sha256)
    {
        return Err(corrupt());
    }
    Ok(())
}

fn decode(encoded: &str) -> Result<StoredConnection> {
    if encoded.len() > 128 * 1024 {
        return Err(corrupt());
    }
    let value: StoredConnection = serde_json::from_str(encoded).map_err(|_| corrupt())?;
    value.descriptor.validate().map_err(|_| corrupt())?;
    if [
        &value.id,
        &value.provider_repository_id,
        &value.credential_ref,
        &value.root_key_ref,
    ]
    .iter()
    .any(|s| s.is_empty())
    {
        return Err(corrupt());
    }
    Ok(value)
}
fn validate_pending(value: &PendingStoredConnection) -> Result<()> {
    if let Some(descriptor) = &value.descriptor {
        descriptor.validate().map_err(|_| corrupt())?;
        if descriptor.repository_id != value.repository_id {
            return Err(corrupt());
        }
    } else if !value.create {
        return Err(corrupt());
    }
    if [&value.id, &value.repository_id, &value.credential_ref, &value.root_key_ref]
        .iter()
        .any(|value| value.is_empty())
        || value.provider_repository_id.as_ref().is_some_and(String::is_empty)
    {
        return Err(corrupt());
    }
    Ok(())
}
fn decode_pending(encoded: &str) -> Result<PendingStoredConnection> {
    if encoded.len() > 128 * 1024 {
        return Err(corrupt());
    }
    let value: PendingStoredConnection = serde_json::from_str(encoded).map_err(|_| corrupt())?;
    validate_pending(&value)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    
    fn pending() -> PendingStoredConnection {
        PendingStoredConnection {
            id: "synthetic-connection".into(),
            config: ConnectionConfig {
                provider: "webdav".into(),
                profile: None,
                endpoint: "https://synthetic.invalid".into(),
                account_id: "synthetic-account".into(),
                location: [("root".into(), "RisuNest".into())].into(),
                oauth_profile: None,
            },
            repository_id: "synthetic-format-repository".into(),
            descriptor: Some(Descriptor::new("synthetic-format-repository".into(), None)
                .unwrap()),
            create: true,
            provider_repository_id: Some("synthetic-provider-repository".into()),
            credential_ref: "provider-v1:00000000-0000-4000-8000-000000000001".into(),
            root_key_ref: "repository-key-v1:00000000-0000-4000-8000-000000000002".into(),
            capture_policy: None,
            created_at_ms: 1,
        }
    }

    fn locator(identity: &str) -> RemoteLocator {
        RemoteLocator {
            connection_identity: identity.into(),
            collection: Some("descriptors".into()),
            object: "descriptor".into(),
        }
    }

    #[test]
    fn retry_keeps_the_pending_repository_key_strategy_and_account() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(root.path()).unwrap();
        let original = pending();
        store.put_pending(&original).unwrap();
        let mutations: [fn(&mut PendingStoredConnection); 7] = [
            |value| {
                value.root_key_ref =
                    "repository-key-v1:00000000-0000-4000-8000-000000000003".into()
            },
            |value| {
                value.repository_id = "different-format-repository".into();
                value.descriptor = Some(
                    Descriptor::new("different-format-repository".into(), None).unwrap(),
                );
            },
            |value| value.create = false,
            |value| value.descriptor.as_mut().unwrap().publication_strategy = Some(PublicationStrategy::Cas),
            |value| value.provider_repository_id = Some("different-root".into()),
            |value| value.config.account_id = "another-account".into(),
            |value| value.config.endpoint = "https://another.invalid".into(),
        ];
        for mutate in mutations {
            let mut retry = original.clone();
            mutate(&mut retry);
            assert_eq!(store.put_pending(&retry).unwrap_err().kind, ErrorKind::PreconditionFailed);
            assert_eq!(serde_json::to_value(store.pending(&original.id).unwrap()).unwrap(),
                serde_json::to_value(&original).unwrap());
        }
        let mut refreshed = original.clone();
        refreshed.credential_ref = "new-credential-for-the-same-account".into();
        store.put_pending(&refreshed).unwrap();
        drop(store);
        let reopened = ConnectionStore::open(root.path()).unwrap();
        let restored = reopened.pending(&original.id).unwrap();
        assert_eq!(restored.credential_ref, refreshed.credential_ref);
        assert_eq!(restored.root_key_ref, original.root_key_ref);
        assert_eq!(restored.descriptor, original.descriptor);
    }

    #[test]
    fn pending_create_is_durable_before_provider_identity_and_strategy_are_known() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(root.path()).unwrap();
        let mut value = pending();
        value.provider_repository_id = None;
        value.descriptor = None;
        store.put_pending(&value).unwrap();
        let mut initialized = value.clone();
        initialized.provider_repository_id = Some("synthetic-provider-repository".into());
        initialized.descriptor = Some(
            Descriptor::new(value.repository_id.clone(), Some(PublicationStrategy::Sequential))
                .unwrap(),
        );
        store.put_pending(&initialized).unwrap();
        let restored = store.pending(&value.id).unwrap();
        assert_eq!(restored.provider_repository_id, initialized.provider_repository_id);
        assert_eq!(restored.descriptor, initialized.descriptor);
        assert_eq!(store.put_pending(&value).unwrap_err().kind, ErrorKind::PreconditionFailed);
    }

    #[test]
    fn a_restarted_prepare_recovers_the_single_matching_create_intent() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(root.path()).unwrap();
        let mut value = pending();
        value.descriptor = None;
        value.provider_repository_id = None;
        store.put_pending(&value).unwrap();
        let matched = store
            .pending_create_for(&value.config, value.capture_policy)
            .unwrap()
            .unwrap();
        assert_eq!(matched.id, value.id);

        let mut duplicate = value.clone();
        duplicate.id = "second-pending-create".into();
        duplicate.credential_ref = "second-credential".into();
        duplicate.root_key_ref = "second-key".into();
        duplicate.repository_id = "second-repository".into();
        store.put_pending(&duplicate).unwrap();
        assert_eq!(
            store
                .pending_create_for(&value.config, value.capture_policy)
                .err()
                .unwrap()
                .kind,
            ErrorKind::Corrupt
        );
    }

    #[test]
    fn pending_create_matching_keeps_credential_accounts_separate() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(root.path()).unwrap();
        let mut first = pending();
        first.config.provider = "mybox".into();
        first.config.profile = Some("plan30gb".into());
        first.config.account_id = crate::external_storage::quota::credential_principal(
            "mybox",
            b"synthetic-pat-a",
        );
        first.config.location = BTreeMap::from([(
            "rootFolderName".into(),
            "RisuNest".into(),
        )]);
        store.put_pending(&first).unwrap();

        let mut second_config = first.config.clone();
        second_config.account_id = crate::external_storage::quota::credential_principal(
            "mybox",
            b"synthetic-pat-b",
        );
        assert!(store
            .pending_create_for(&second_config, first.capture_policy)
            .unwrap()
            .is_none());
        assert_eq!(
            store
                .pending_create_for(&first.config, first.capture_policy)
                .unwrap()
                .unwrap()
                .id,
            first.id
        );
    }

    #[test]
    fn only_real_completions_set_independent_monotonic_timestamps() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(root.path()).unwrap();
        let mut value = pending();
        value.descriptor.as_mut().unwrap().publication_strategy = Some(PublicationStrategy::Sequential);
        store.put_pending(&value).unwrap();
        let connected = store.promote_pending(&value.id, locator("synthetic"), Capabilities::default()).unwrap();
        assert_eq!(connected.last_sync_at_ms, None);
        assert_eq!(connected.last_backup_at_ms, None);
        store.record_completion(&value.id, CompletionKind::Backup, 20).unwrap();
        assert_eq!(store.read(&value.id).unwrap().last_sync_at_ms, None);
        store.record_completion(&value.id, CompletionKind::Sync, 30).unwrap();
        store.record_completion(&value.id, CompletionKind::Sync, 10).unwrap();
        assert_eq!(store.record_completion(&value.id, CompletionKind::Backup, 0).unwrap_err().kind, ErrorKind::Corrupt);
        drop(store);
        let restored = ConnectionStore::open(root.path()).unwrap().read(&value.id).unwrap();
        assert_eq!(restored.last_sync_at_ms, Some(30));
        assert_eq!(restored.last_backup_at_ms, Some(20));
        assert_eq!(restored.descriptor.publication_strategy, Some(PublicationStrategy::Sequential));
    }

    #[test]
    fn discovery_checks_the_current_repository_role_id_and_hash() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(root.path()).unwrap();
        let value = pending();
        store.put_pending(&value).unwrap();
        store.promote_pending(&value.id, locator("synthetic"), Capabilities::default()).unwrap();
        let reference = RemoteObject {
            repository_id: value.repository_id.clone(),
            object_id: "snapshot-a".into(),
            role: ObjectRole::BackupBundle,
            receipt: ObjectReceipt {
                locator: RemoteLocator { connection_identity: "synthetic".into(), collection: None, object: "bundle-a".into() },
                byte_length: 100, version: None, checksum: None, complete: true,
            },
            ciphertext_sha256: "a".repeat(64), plaintext_sha256: "b".repeat(64), plaintext_length: 50,
        };
        store.remember_discovery(&value.id, "a", &reference).unwrap();
        assert_eq!(store.discovery_snapshot(&value.id, "a").unwrap(), reference);
        assert_eq!(store.discovery(&value.id, "a", ObjectRole::BackupBundle,
            Some(&reference.ciphertext_sha256)).unwrap(), reference);
        assert_eq!(store.discovery(&value.id, "a", ObjectRole::SyncState, None).unwrap_err().kind, ErrorKind::Corrupt);
        assert_eq!(store.discovery(&value.id, "a", ObjectRole::BackupBundle, Some(&"c".repeat(64)))
            .unwrap_err().kind, ErrorKind::Corrupt);
        for mutate in [
            (|object: &mut RemoteObject| object.repository_id = "other-repository".into()) as fn(&mut RemoteObject),
            |object| object.object_id = "snapshot-b".into(),
            |object| object.receipt.locator.connection_identity = "other-connection".into(),
            |object| object.role = ObjectRole::Pack,
            |object| object.receipt.complete = false,
            |object| object.ciphertext_sha256 = "invalid".into(),
        ] {
            let mut invalid = reference.clone();
            mutate(&mut invalid);
            assert_eq!(store.remember_discovery(&value.id, "a", &invalid).unwrap_err().kind, ErrorKind::Corrupt);
        }
        let mut state = reference.clone();
        state.object_id = "snapshot-b".into();
        state.role = ObjectRole::SyncState;
        state.receipt.locator.object = "state-b".into();
        store.remember_discovery(&value.id, "b", &state).unwrap();
        assert_eq!(store.discovery_snapshot(&value.id, "b").unwrap(), state);
        store.forget_discovery(&value.id, "a").unwrap();
        store.forget_discovery(&value.id, "a").unwrap();
        assert_eq!(store.discovery_snapshot(&value.id, "a").unwrap_err().kind, ErrorKind::NotFound);
        assert_eq!(store.discovery_snapshot(&value.id, "b").unwrap(), state);
        // A cache row is not enough once its connection has been removed.
        store.remove(&value.id).unwrap();
        assert_eq!(store.discovery_snapshot(&value.id, "b").unwrap_err().kind, ErrorKind::NotFound);
    }

    #[test]
    fn pending_connection_is_invisible_until_atomic_promotion() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(root.path()).unwrap();
        let pending = pending();
        store.put_pending(&pending).unwrap();

        assert!(store.list().unwrap().is_empty());
        assert_eq!(store.pending(&pending.id).unwrap().id, pending.id);

        let locator = locator("synthetic-identity");
        let stored = store
            .promote_pending(&pending.id, locator.clone(), Capabilities::default())
            .unwrap();
        assert_eq!(stored.descriptor_locator, locator);
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(matches!(
            store.pending(&pending.id),
            Err(ProviderError {
                kind: ErrorKind::NotFound,
                ..
            })
        ));
    }
    /// A second connection to one repository would apply its own retention to
    /// the backups the first one made, so the store refuses it.
    #[test]
    fn one_repository_holds_one_connection() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(root.path()).unwrap();
        let first = pending();
        store.put_pending(&first).unwrap();
        store
            .promote_pending(&first.id, locator("shared"), Capabilities::default())
            .unwrap();

        let mut second = pending();
        second.id = "synthetic-second".into();
        store.put_pending(&second).unwrap();
        assert!(matches!(
            store.promote_pending(&second.id, locator("shared"), Capabilities::default()),
            Err(ProviderError {
                kind: ErrorKind::PreconditionFailed,
                ..
            })
        ));
        assert_eq!(
            store.identity_holder("shared").unwrap().as_deref(),
            Some(first.id.as_str())
        );
        assert_eq!(store.identity_holder("elsewhere").unwrap(), None);
        assert_eq!(store.list().unwrap().len(), 1);

        let promoted = store
            .promote_pending(&second.id, locator("elsewhere"), Capabilities::default())
            .unwrap();
        assert_eq!(promoted.id, second.id);
        assert!(matches!(
            store.insert(&promoted),
            Err(ProviderError {
                kind: ErrorKind::PreconditionFailed,
                ..
            })
        ));
    }
    /// A synchronization connection holds backup points too, so it carries a
    /// retention policy as well. Until one is set the connection has none and
    /// the default applies.
    #[test]
    fn every_connection_kind_carries_a_retention_policy() {
        use super::super::connection::RetentionPolicy;

        let root = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(root.path()).unwrap();
        let sync = pending();
        store.put_pending(&sync).unwrap();
        store
            .promote_pending(&sync.id, locator("synthetic"), Capabilities::default())
            .unwrap();
        assert_eq!(store.read(&sync.id).unwrap().retention_policy, None);

        let narrowed = RetentionPolicy {
            keep_count: 3,
            keep_days: RetentionPolicy::MIN_KEEP_DAYS,
        };
        let updated = store.set_retention_policy(&sync.id, narrowed).unwrap();
        assert_eq!(updated.retention_policy, Some(narrowed));
        assert_eq!(
            store.read(&sync.id).unwrap().retention_policy,
            Some(narrowed)
        );

        assert!(matches!(
            store.set_retention_policy(
                &sync.id,
                RetentionPolicy {
                    keep_count: 3,
                    keep_days: RetentionPolicy::MIN_KEEP_DAYS - 1,
                },
            ),
            Err(ProviderError {
                kind: ErrorKind::PreconditionFailed,
                ..
            })
        ));
        assert_eq!(
            store.read(&sync.id).unwrap().retention_policy,
            Some(narrowed)
        );
    }
    /// Changing a backup connection's policy applies to work started later. A
    /// synchronization connection has none to change.
    #[test]
    fn a_backup_policy_changes_in_place_and_a_sync_connection_has_none() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(root.path()).unwrap();
        let mut backup = pending();
        backup.capture_policy = Some(super::super::connection::CapturePolicy::default());
        store.put_pending(&backup).unwrap();
        store
            .promote_pending(
                &backup.id,
                locator("synthetic-backup"),
                Capabilities::default(),
            )
            .unwrap();

        let narrowed = super::super::connection::CapturePolicy {
            hypa: false,
            local_plugins: true,
            local_settings: false,
        };
        let updated = store.set_capture_policy(&backup.id, narrowed).unwrap();
        assert_eq!(updated.capture_policy, Some(narrowed));
        assert_eq!(
            store.read(&backup.id).unwrap().capture_policy,
            Some(narrowed)
        );

        let mut sync = pending();
        sync.id = "synthetic-sync".into();
        sync.capture_policy = None;
        store.put_pending(&sync).unwrap();
        store
            .promote_pending(&sync.id, locator("synthetic-sync"), Capabilities::default())
            .unwrap();
        assert!(matches!(
            store.set_capture_policy(&sync.id, narrowed),
            Err(ProviderError {
                kind: ErrorKind::Unsupported,
                ..
            })
        ));
    }
}
