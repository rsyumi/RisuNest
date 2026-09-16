//! Plugin consent kept per device. A grant is recorded against the script hash
//! that was consented to, while the reconfirmation clock is kept per plugin
//! name, so the two live in their own tables instead of one mixed keyspace.
use super::{invalid, DeviceStore};
use crate::persistent_store::StoreResult;
use rusqlite::params;

const MAX_NAME_BYTES: usize = 256;
const MAX_HASH_BYTES: usize = 256;
const MAX_PERMISSION_BYTES: usize = 64;

pub(crate) struct PluginPermission {
    pub(crate) code_hash: String,
    pub(crate) permission: String,
    pub(crate) granted: bool,
}

pub(crate) struct PluginPermissionGrant {
    pub(crate) plugin_name: String,
    pub(crate) permission: String,
    pub(crate) last_grant_at: i64,
}

fn check_bounded(value: &str, limit: usize, message: &'static str) -> StoreResult<()> {
    if value.is_empty() || value.len() > limit {
        return Err(invalid(message));
    }
    Ok(())
}

impl DeviceStore {
    /// One read covers every plugin. The renderer still hashes the current
    /// script and judges the permission again; this only says what was granted.
    pub(crate) fn read_plugin_permissions(&self) -> StoreResult<Vec<PluginPermission>> {
        let mut statement = self.connection.prepare(
            "SELECT code_hash,permission,granted FROM plugin_permissions
                ORDER BY code_hash,permission",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(PluginPermission {
                    code_hash: row.get(0)?,
                    permission: row.get(1)?,
                    granted: row.get::<_, i64>(2)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn read_plugin_permission_grants(&self) -> StoreResult<Vec<PluginPermissionGrant>> {
        let mut statement = self.connection.prepare(
            "SELECT plugin_name,permission,last_grant_at FROM plugin_permission_grants
                ORDER BY plugin_name,permission",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(PluginPermissionGrant {
                    plugin_name: row.get(0)?,
                    permission: row.get(1)?,
                    last_grant_at: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn write_plugin_permission(
        &self,
        code_hash: &str,
        permission: &str,
        granted: bool,
    ) -> StoreResult<()> {
        check_bounded(code_hash, MAX_HASH_BYTES, "plugin code hash is invalid")?;
        check_bounded(
            permission,
            MAX_PERMISSION_BYTES,
            "plugin permission is invalid",
        )?;
        self.connection.execute(
            "INSERT INTO plugin_permissions (code_hash,permission,granted) VALUES (?1,?2,?3)
                ON CONFLICT(code_hash,permission) DO UPDATE SET granted=excluded.granted",
            params![code_hash, permission, i64::from(granted)],
        )?;
        Ok(())
    }

    pub(crate) fn write_plugin_permission_grant(
        &self,
        plugin_name: &str,
        permission: &str,
        last_grant_at: i64,
    ) -> StoreResult<()> {
        check_bounded(plugin_name, MAX_NAME_BYTES, "plugin name is invalid")?;
        check_bounded(
            permission,
            MAX_PERMISSION_BYTES,
            "plugin permission is invalid",
        )?;
        if last_grant_at < 0 {
            return Err(invalid("plugin grant time is out of range"));
        }
        self.connection.execute(
            "INSERT INTO plugin_permission_grants (plugin_name,permission,last_grant_at)
                VALUES (?1,?2,?3)
                ON CONFLICT(plugin_name,permission)
                DO UPDATE SET last_grant_at=excluded.last_grant_at",
            params![plugin_name, permission, last_grant_at],
        )?;
        Ok(())
    }
}
