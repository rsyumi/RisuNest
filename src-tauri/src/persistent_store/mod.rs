pub(crate) mod commands;
mod commit;
mod export;
mod query;
mod schema;
mod snapshot;

pub(crate) use commands::PersistentStoreState;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub(super) type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, Serialize, PartialEq, Eq)]
#[serde(tag = "code", rename_all = "kebab-case")]
pub(crate) enum StoreError {
    RevisionConflict {
        expected: i64,
        actual: i64,
    },
    SnapshotReleased,
    Validation {
        message: String,
    },
    #[serde(rename = "store-error")]
    Store {
        message: String,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RevisionConflict { expected, actual } => {
                write!(
                    formatter,
                    "expected data revision {expected}, but current revision is {actual}"
                )
            }
            Self::SnapshotReleased => {
                formatter.write_str("persistent revision snapshot has been released")
            }
            Self::Validation { message } | Self::Store { message } => formatter.write_str(message),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store {
            message: error.to_string(),
        }
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Store {
            message: error.to_string(),
        }
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Store {
            message: error.to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Versioned<T> {
    pub(crate) revision: i64,
    pub(crate) value: T,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RevisionResult {
    pub(crate) revision: i64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum QueryOrder {
    Configured,
    Recent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CharacterQuery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) search: Option<String>,
    pub(crate) order: QueryOrder,
    pub(crate) trash: bool,
    pub(crate) limit: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CharacterSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) image: Option<String>,
    pub(crate) configured_index: i64,
    pub(crate) recent_at: i64,
    pub(crate) trashed: bool,
    pub(crate) conversation_count: i64,
    pub(crate) r#type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) creator_notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) trash_time: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PresetSummary {
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) image: Option<String>,
    pub(crate) configured_index: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PresetCatalog {
    pub(crate) revision: i64,
    pub(crate) items: Vec<PresetSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CharacterPage {
    pub(crate) revision: i64,
    pub(crate) items: Vec<CharacterSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationQuery {
    pub(crate) character_id: String,
    pub(crate) order: QueryOrder,
    pub(crate) limit: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationSummary {
    pub(crate) id: String,
    pub(crate) character_id: String,
    pub(crate) name: String,
    pub(crate) configured_index: i64,
    pub(crate) recent_at: i64,
    pub(crate) message_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationPage {
    pub(crate) revision: i64,
    pub(crate) items: Vec<ConversationSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) next_cursor: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationWindowQuery {
    pub(crate) character_id: String,
    pub(crate) conversation_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) limit: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) anchor_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) before: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) after: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConversationWindow {
    pub(crate) character_id: String,
    pub(crate) conversation_id: String,
    pub(crate) messages: Vec<Value>,
    pub(crate) start_index: i64,
    pub(crate) end_index: i64,
    pub(crate) total_messages: i64,
    pub(crate) has_more_before: bool,
    pub(crate) has_more_after: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub(crate) enum ConversationMutation {
    ReplaceRange {
        character_id: String,
        conversation_id: String,
        start: i64,
        delete_count: i64,
        messages: Vec<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        conversation: Option<Value>,
    },
    Delete {
        character_id: String,
        conversation_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WorkingSetCommit {
    pub(crate) expected_revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) root: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) replace_presets: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) character: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) replace_character: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) add_character: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) conversations: Option<Vec<ConversationMutation>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) delete_character_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LeaseResult {
    pub(crate) lease: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StagingResult {
    pub(crate) staging_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotInfo {
    pub(crate) path: String,
    pub(crate) bytes: u64,
    pub(crate) modified_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotCreated {
    pub(crate) path: String,
    pub(crate) bytes: u64,
    pub(crate) duration_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) enum CheckpointMode {
    Passive,
    Truncate,
}

pub(super) struct ReadTarget {
    pub(super) revision: i64,
    pub(super) generation: String,
}

pub(crate) struct PersistentStore {
    connection: Connection,
    snapshots_dir: PathBuf,
}

impl PersistentStore {
    pub(crate) fn open(app_data_dir: &Path) -> StoreResult<Self> {
        let persistent_dir = app_data_dir.join("persistent");
        let snapshots_dir = persistent_dir.join("snapshots");
        std::fs::create_dir_all(&snapshots_dir)?;
        snapshot::apply_pending_restore(&persistent_dir, &snapshots_dir)?;

        let database_path = persistent_dir.join("persistent.db");
        let mut connection = Connection::open(&database_path)?;
        schema::initialize(&mut connection)?;

        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
            params!["currentRevision", "0"],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES (?1, ?2)",
            params!["activeGeneration", "\"revision-0\""],
        )?;
        transaction.execute(
            "INSERT OR IGNORE INTO root (generation, value) VALUES (?1, ?2)",
            params!["revision-0", "{}"],
        )?;
        transaction.commit()?;
        export::sweep_abandoned(&mut connection, &snapshots_dir)?;
        snapshot::sweep_temporary_generations(&mut connection)?;

        Ok(Self {
            connection,
            snapshots_dir,
        })
    }

    pub(crate) fn revision(&self) -> StoreResult<i64> {
        current_revision(&self.connection)
    }

    pub(crate) fn read_root(&self, lease: Option<&str>) -> StoreResult<Versioned<Value>> {
        query::read_root(&self.connection, lease)
    }

    pub(crate) fn query_presets(&self, lease: Option<&str>) -> StoreResult<PresetCatalog> {
        query::query_presets(&self.connection, lease)
    }

    pub(crate) fn read_preset(
        &self,
        id: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<Value>>> {
        query::read_preset(&self.connection, id, lease)
    }

    pub(crate) fn query_characters(
        &self,
        query: &CharacterQuery,
        lease: Option<&str>,
    ) -> StoreResult<CharacterPage> {
        query::query_characters(&self.connection, query, lease)
    }

    pub(crate) fn read_character(
        &self,
        id: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<Value>>> {
        query::read_character(&self.connection, id, lease)
    }

    pub(crate) fn query_conversations(
        &self,
        query: &ConversationQuery,
        lease: Option<&str>,
    ) -> StoreResult<ConversationPage> {
        query::query_conversations(&self.connection, query, lease)
    }

    pub(crate) fn read_conversation(
        &self,
        character_id: &str,
        conversation_id: &str,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<Value>>> {
        query::read_conversation(&self.connection, character_id, conversation_id, lease)
    }

    pub(crate) fn read_conversation_window(
        &self,
        query: &ConversationWindowQuery,
        lease: Option<&str>,
    ) -> StoreResult<Option<Versioned<ConversationWindow>>> {
        query::read_conversation_window(&self.connection, query, lease)
    }

    pub(crate) fn materialize(&self, revision: Option<i64>) -> StoreResult<Value> {
        query::materialize(&self.connection, revision)
    }

    pub(crate) fn commit(&mut self, commit: &WorkingSetCommit) -> StoreResult<RevisionResult> {
        commit::commit(&mut self.connection, commit)
    }

    pub(crate) fn replace_begin(&mut self) -> StoreResult<StagingResult> {
        commit::replace_begin(&mut self.connection)
    }

    pub(crate) fn replace_put_root(&mut self, staging_id: &str, root: &Value) -> StoreResult<()> {
        commit::replace_put_root(&mut self.connection, staging_id, root)
    }

    pub(crate) fn replace_put_presets(
        &mut self,
        staging_id: &str,
        presets: &[Value],
    ) -> StoreResult<()> {
        commit::replace_put_presets(&mut self.connection, staging_id, presets)
    }

    pub(crate) fn replace_add_characters(
        &mut self,
        staging_id: &str,
        characters: &[Value],
    ) -> StoreResult<()> {
        commit::replace_add_characters(&mut self.connection, staging_id, characters)
    }

    pub(crate) fn replace_commit(
        &mut self,
        staging_id: &str,
        expected_revision: Option<i64>,
    ) -> StoreResult<RevisionResult> {
        let revision =
            commit::validate_replace_commit(&self.connection, staging_id, expected_revision)?;
        if revision > 0 {
            snapshot::create(&self.connection, &self.snapshots_dir, "pre-replace")?;
        }
        commit::replace_commit(&mut self.connection, staging_id, expected_revision)
    }

    pub(crate) fn replace_abort(&mut self, staging_id: &str) -> StoreResult<()> {
        commit::replace_abort(&mut self.connection, staging_id)
    }

    pub(crate) fn acquire_revision(&mut self, revision: i64) -> StoreResult<LeaseResult> {
        snapshot::acquire_revision(&mut self.connection, revision)
    }

    pub(crate) fn release_revision(&mut self, lease: &str) -> StoreResult<()> {
        snapshot::release_revision(&mut self.connection, lease)
    }

    pub(crate) fn export_risu_save(
        &self,
        lease: &str,
        omit_account: bool,
    ) -> StoreResult<export::ExportedRisuSave> {
        export::create(&self.connection, &self.snapshots_dir, lease, omit_account)
    }

    pub(crate) fn cleanup_risu_save_export(&self, path: &Path) -> StoreResult<()> {
        export::cleanup(&self.snapshots_dir, path)
    }

    pub(crate) fn checkpoint(&self, mode: CheckpointMode) -> StoreResult<()> {
        snapshot::checkpoint(&self.connection, mode)
    }

    pub(crate) fn snapshot_create(&self, reason: &str) -> StoreResult<SnapshotCreated> {
        snapshot::create(&self.connection, &self.snapshots_dir, reason)
    }

    pub(crate) fn snapshot_list(&self) -> StoreResult<Vec<SnapshotInfo>> {
        snapshot::list(&self.snapshots_dir)
    }

    pub(crate) fn snapshot_restore_request(&self, path: &Path) -> StoreResult<()> {
        snapshot::restore_request(&self.snapshots_dir, path)
    }

    pub(crate) fn get_app_kv(&self, key: &str) -> StoreResult<Option<Value>> {
        let value: Option<String> = self
            .connection
            .query_row("SELECT value FROM app_kv WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?;
        value
            .map(|value| serde_json::from_str(&value).map_err(StoreError::from))
            .transpose()
    }

    pub(crate) fn set_app_kv(&self, key: &str, value: &Value) -> StoreResult<()> {
        self.connection.execute(
            "INSERT INTO app_kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, serde_json::to_string(value)?],
        )?;
        Ok(())
    }

    pub(crate) fn remove_app_kv(&self, key: &str) -> StoreResult<()> {
        self.connection
            .execute("DELETE FROM app_kv WHERE key = ?1", [key])?;
        Ok(())
    }
}

pub(super) fn current_revision(connection: &Connection) -> StoreResult<i64> {
    let value: String = connection.query_row(
        "SELECT value FROM meta WHERE key = 'currentRevision'",
        [],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&value)?)
}

pub(super) fn active_generation(connection: &Connection) -> StoreResult<String> {
    let value: String = connection.query_row(
        "SELECT value FROM meta WHERE key = 'activeGeneration'",
        [],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&value)?)
}

pub(super) fn read_target(connection: &Connection, lease: Option<&str>) -> StoreResult<ReadTarget> {
    match lease {
        None => Ok(ReadTarget {
            revision: current_revision(connection)?,
            generation: active_generation(connection)?,
        }),
        Some(generation) => {
            let exists = connection
                .query_row(
                    "SELECT 1 FROM snapshot_leases WHERE generation = ?1",
                    [generation],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !exists {
                return Err(StoreError::SnapshotReleased);
            }
            let revision = generation
                .strip_prefix("snapshot-")
                .and_then(|value| value.split('-').next())
                .and_then(|value| value.parse::<i64>().ok())
                .ok_or(StoreError::SnapshotReleased)?;
            Ok(ReadTarget {
                revision,
                generation: generation.to_owned(),
            })
        }
    }
}

#[cfg(test)]
mod benchmark;
#[cfg(test)]
mod tests;
