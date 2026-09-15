//! Device-local connection settings. Only opaque OS-vault references are stored
//! here; sync bases and job outcomes remain authoritative in the library PDS.
use super::{capabilities::Capabilities, contract::*};
use risunest_external_storage_format::format::Descriptor;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;

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
    pub fn insert(&mut self, connection: &StoredConnection) -> Result<()> {
        connection.descriptor.validate().map_err(|_| corrupt())?;
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
    use risunest_external_storage_format::format::Scope;

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

    #[test]
    fn pending_connection_is_invisible_until_atomic_promotion() {
        let root = tempfile::tempdir().unwrap();
        let mut store = ConnectionStore::open(root.path()).unwrap();
        let pending = pending();
        store.put_pending(&pending).unwrap();

        assert!(store.list().unwrap().is_empty());
        assert_eq!(store.pending(&pending.id).unwrap().id, pending.id);

        let locator = RemoteLocator {
            connection_identity: "synthetic-identity".into(),
            collection: Some("descriptors".into()),
            object: "descriptor".into(),
        };
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
}
