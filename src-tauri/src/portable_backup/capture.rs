use super::*;
use crate::{
    asset_repository::{job_pins::DurableCasJob, PayloadCas},
    persistent_store::PersistentStore,
};
use std::path::Path;

pub(crate) struct CapturedLibrary {
    pub(crate) catalog: Catalog,
    pub(crate) repair_required: bool,
}

/// A failed normal capture is discarded before calling again with preservation=true. Each call
/// establishes a fresh lease and immutable physical inventory. I/O and cancellation never become
/// successful preservation merely because a normal attempt failed.
pub(crate) fn capture_library(
    store: &mut PersistentStore,
    revision: i64,
    job_directory: &Path,
    pins: &mut DurableCasJob,
    preservation: bool,
    probe: &dyn CancellationProbe,
) -> Result<CapturedLibrary> {
    check(probe)?;
    store
        .hydrate_registered_remote_assets(|| {
            if probe.is_cancelled() {
                Err(crate::server_sync::SyncError::new("cancelled", 409))
            } else {
                Ok(())
            }
        })
        .map_err(|error| {
            if error.code == "cancelled" {
                Error::Cancelled
            } else {
                Error::Io(std::io::Error::other(
                    "complete remote asset download required before backup",
                ))
            }
        })?;
    let lease = store.acquire_revision(revision)?.lease;
    let outcome = (|| {
        let mut catalog = Catalog::create(job_directory, env!("CARGO_PKG_VERSION"), revision)?;
        let generation = store.portable_source_generation(&lease)?;
        catalog.db.execute(
            "INSERT INTO backup_info VALUES('sourceGeneration',?1)",
            [&generation],
        )?;
        if let Err(error) = store.capture_portable_records(&lease, &mut catalog.db, probe) {
            check(probe)?;
            if !matches!(error, StoreError::Validation { .. }) {
                return Err(error.into());
            }
            if !preservation {
                return Err(Error::SourceNeedsPreservation);
            }
            // Unknown raw schema is the explicit physical SQLite salvage profile. This snapshot
            // may contain operational tables and is never eligible for normal activation.
            if !matches!(error, StoreError::Validation { .. }) {
                return Err(error.into());
            }
            let source = catalog.directory.path().join("source.sqlite");
            store.capture_preservation_database(&lease, &source, probe)?;
            catalog.db.execute(
                "UPDATE backup_info SET value='source-sqlite' WHERE key='profile'",
                [],
            )?;
            catalog.add_file(
                "preserved",
                "source.sqlite",
                "{\"profile\":\"source-sqlite\"}",
                Some((&source, std::fs::metadata(&source)?.len())),
                None,
                probe,
            )?;
        }
        catalog.begin_inventory()?;
        let cas = PayloadCas::new(store.repository_root())?;
        match catalog.capture_files(store, &cas, pins, preservation, probe) {
            Err(Error::Invalid("registered source payload differs")) if !preservation => {
                return Err(Error::SourceNeedsPreservation)
            }
            result => result?,
        }
        let validation = catalog.validate_library(probe);
        check(probe)?;
        let repair_required = match validation {
            Ok(_) => false,
            Err(
                Error::Invalid(_) | Error::Json(_) | Error::Store(StoreError::Validation { .. }),
            ) => true,
            Err(error) => return Err(error),
        };
        if repair_required {
            if !preservation {
                return Err(Error::SourceNeedsPreservation);
            }
            catalog.db.execute(
                "INSERT INTO diagnostics VALUES('source-preserved-repair-required','library')",
                [],
            )?;
        }
        Ok(CapturedLibrary {
            catalog,
            repair_required,
        })
    })();
    let release = store.release_revision(&lease);
    match (outcome, release) {
        (Ok(capture), Ok(_)) => Ok(capture),
        (Err(error), Ok(_)) => Err(error),
        (_, Err(error)) => Err(error.into()),
    }
}
