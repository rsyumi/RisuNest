//! Raw source capture deliberately does not materialize application JSON.
use super::*;
use crate::local_backup::CancellationProbe;
use std::path::Path;

impl PersistentStore {
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
