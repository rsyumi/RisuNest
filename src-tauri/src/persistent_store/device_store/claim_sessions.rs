//! The one window in which a plugin may take a value an upstream save left
//! without an owner. The window belongs to a single run of a single plugin and
//! is never reopened, so a restart cannot take a value a person already decided
//! against.
use super::{invalid, DeviceStore};
use crate::persistent_store::StoreResult;
use rusqlite::{params, OptionalExtension};
use uuid::Uuid;

/// A safety bound on the window, not a promise about how long a plugin needs.
pub(crate) const CLAIM_SESSION_LIMIT_MS: i64 = 30_000;

fn validate(field: &str, value: &str) -> StoreResult<()> {
    if value.is_empty() || value.len() > 512 {
        return Err(invalid(&format!("plugin claim {field} is invalid")));
    }
    Ok(())
}

impl DeviceStore {
    /// Opens the window for one plugin run. Answers with nothing when this
    /// plugin already had its window for this import, which is what keeps a
    /// restart or an ordinary plugin reload from opening a second one.
    pub(crate) fn open_plugin_claim_session(
        &self,
        import_batch_id: &str,
        owner: &str,
        code_hash: &str,
        runtime_instance: &str,
        now_ms: i64,
    ) -> StoreResult<Option<String>> {
        validate("import batch", import_batch_id)?;
        validate("owner", owner)?;
        validate("code hash", code_hash)?;
        validate("runtime instance", runtime_instance)?;
        let taken: i64 = self.connection.query_row(
            "SELECT count(*) FROM plugin_claim_sessions
                WHERE import_batch_id=?1 AND owner=?2 AND code_hash=?3",
            params![import_batch_id, owner, code_hash],
            |row| row.get(0),
        )?;
        if taken != 0 {
            return Ok(None);
        }
        let session_id = Uuid::new_v4().to_string();
        self.connection.execute(
            "INSERT INTO plugin_claim_sessions
                (session_id,import_batch_id,owner,code_hash,runtime_instance,started_at,
                 expires_at,closed)
                VALUES (?1,?2,?3,?4,?5,?6,?7,0)",
            params![
                session_id,
                import_batch_id,
                owner,
                code_hash,
                runtime_instance,
                now_ms,
                now_ms + CLAIM_SESSION_LIMIT_MS
            ],
        )?;
        Ok(Some(session_id))
    }

    /// The import this session may take from, or nothing when the window is
    /// closed, expired, or was opened for a different caller.
    pub(crate) fn plugin_claim_session_batch(
        &self,
        session_id: &str,
        owner: &str,
        code_hash: &str,
        runtime_instance: &str,
        now_ms: i64,
    ) -> StoreResult<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT import_batch_id FROM plugin_claim_sessions
                    WHERE session_id=?1 AND owner=?2 AND code_hash=?3 AND runtime_instance=?4
                      AND closed=0 AND expires_at>?5",
                params![session_id, owner, code_hash, runtime_instance, now_ms],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub(crate) fn close_plugin_claim_session(&self, session_id: &str) -> StoreResult<()> {
        self.connection.execute(
            "UPDATE plugin_claim_sessions SET closed=1 WHERE session_id=?1",
            [session_id],
        )?;
        Ok(())
    }
}
