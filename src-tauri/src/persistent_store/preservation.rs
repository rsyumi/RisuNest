//! Raw source capture deliberately does not materialize application JSON.
use super::*;
use crate::local_backup::CancellationProbe;
use std::path::Path;

impl PersistentStore {
    pub(crate) fn has_unrepresentable_source_records(&self, lease: &str) -> StoreResult<bool> {
        let (source, target) = self.read_view(Some(lease))?;
        Ok(source.query_row(
            "SELECT EXISTS (SELECT 1 FROM conversations c WHERE c.generation=?1 AND NOT EXISTS (
                SELECT 1 FROM characters p WHERE p.generation=c.generation AND p.character_id=c.character_id))
             OR EXISTS (SELECT 1 FROM messages m WHERE m.generation=?1 AND NOT EXISTS (
                SELECT 1 FROM conversations p WHERE p.generation=m.generation AND p.character_id=m.character_id AND p.conversation_id=m.conversation_id))
             OR EXISTS (SELECT 1 FROM root WHERE generation=?1 AND CASE WHEN json_valid(value) THEN
                json_type(value, '$.characters') IS NOT NULL OR json_type(value, '$.botPresets') IS NOT NULL
                OR json_type(value, '$.pluginCustomStorage') IS NOT NULL ELSE 1 END)
             OR EXISTS (SELECT 1 FROM characters WHERE generation=?1 AND CASE WHEN json_valid(detail) THEN
                json_type(detail, '$.chats') IS NOT NULL OR json_extract(detail, '$.chaId') IS NOT character_id ELSE 1 END)
             OR EXISTS (SELECT 1 FROM conversations WHERE generation=?1 AND CASE WHEN json_valid(detail) THEN
                json_type(detail, '$.message') IS NOT NULL OR json_extract(detail, '$.id') IS NOT conversation_id ELSE 1 END)",
            [&target.generation], |row| row.get(0),
        )?)
    }

    pub(crate) fn capture_preservation_database(
        &self,
        lease: &str,
        destination: &Path,
        cancellation: &dyn CancellationProbe,
    ) -> StoreResult<(u64, u64)> {
        let (source, target) = self.read_view(Some(lease))?;
        let mut output = Connection::open(destination)?;
        {
            let backup = rusqlite::backup::Backup::new(source, &mut output)?;
            loop {
                if cancellation.is_cancelled() {
                    return Err(StoreError::Validation {
                        message: "source preservation cancelled".into(),
                    });
                }
                match backup.step(128)? {
                    rusqlite::backup::StepResult::Done => break,
                    rusqlite::backup::StepResult::More => (),
                    _ => {
                        return Err(StoreError::Store {
                            message: "source preservation database is busy".into(),
                        })
                    }
                }
            }
        }
        let characters = output.query_row(
            "SELECT count(*) FROM characters WHERE generation=?1",
            [&target.generation],
            |row| row.get::<_, i64>(0),
        )?;
        let presets = output.query_row(
            "SELECT count(*) FROM bot_presets WHERE generation=?1",
            [&target.generation],
            |row| row.get::<_, i64>(0),
        )?;
        output
            .close()
            .map_err(|(_, error)| StoreError::from(error))?;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(destination)?
            .sync_all()?;
        Ok((characters as u64, presets as u64))
    }
}
