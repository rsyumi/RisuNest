//! Device-local connection settings. Only opaque OS-vault references are stored
//! here; sync bases and job outcomes remain authoritative in the library PDS.
use super::{capabilities::Capabilities, contract::*};
use risunest_external_storage_format::format::Descriptor;
use rusqlite::{params, Connection, OptionalExtension};
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
    /// Backup connections only. Changing it applies to work started
    /// afterwards and never rewrites an existing point.
    #[serde(default)]
    pub capture_policy: Option<super::connection::CapturePolicy>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PendingStoredConnection {
    pub id: String,
    pub config: ConnectionConfig,
    pub descriptor: Descriptor,
    #[serde(default)]
    pub capture_policy: Option<super::connection::CapturePolicy>,
    pub provider_repository_id: Option<String>,
    pub credential_ref: String,
    pub root_key_ref: String,
    pub created_at_ms: u64,
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
    pub fn put_pending(&self, connection: &PendingStoredConnection) -> Result<()> {
        validate_pending(connection)?;
        let encoded = serde_json::to_string(connection).map_err(storage)?;
        self.0.execute(
            "INSERT INTO pending_connections VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET value=excluded.value",
            params![connection.id, encoded],
        ).map_err(storage)?;
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
    pub fn promote_pending(
        &mut self,
        id: &str,
        descriptor_locator: RemoteLocator,
        capabilities: Capabilities,
    ) -> Result<StoredConnection> {
        let pending = self.pending(id)?;
        let provider_repository_id = pending.provider_repository_id.ok_or_else(corrupt)?;
        let connection = StoredConnection {
            id: pending.id,
            config: pending.config,
            descriptor: pending.descriptor,
            descriptor_locator,
            provider_repository_id,
            credential_ref: pending.credential_ref,
            capture_policy: pending.capture_policy,
            root_key_ref: pending.root_key_ref,
            capabilities,
            created_at_ms: pending.created_at_ms,
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
    pub fn remember_discovery<T: Serialize>(
        &self,
        connection: &str,
        id: &str,
        value: &T,
    ) -> Result<()> {
        let encoded = serde_json::to_string(value).map_err(storage)?;
        if encoded.len() > 256 * 1024 {
            return Err(corrupt());
        }
        self.0.execute("INSERT INTO discovery VALUES(?1,?2,?3) ON CONFLICT(connection_id,id) DO UPDATE SET value=excluded.value",
            params![connection, id, encoded]).map_err(storage)?;
        Ok(())
    }
    pub fn discovery<T: serde::de::DeserializeOwned>(
        &self,
        connection: &str,
        id: &str,
    ) -> Result<T> {
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
        serde_json::from_str(&encoded).map_err(|_| corrupt())
    }
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
    value.descriptor.validate().map_err(|_| corrupt())?;
    if [&value.id, &value.credential_ref, &value.root_key_ref]
        .iter()
        .any(|value| value.is_empty())
        || value
            .provider_repository_id
            .as_ref()
            .is_some_and(String::is_empty)
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
            descriptor: Descriptor::new("synthetic-format-repository".into(), None,
            )
            .unwrap(),
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
