//! Library-only adapter for internal conflict preservation. No renderer maintenance is involved.
use super::*;
use crate::asset_repository::job_pins::{CasJobKind, CasReleaseOutcome, DurableCasJob};
use crate::persistent_store::PersistentStore;
use std::{fs::File, path::Path};

pub(crate) fn create_verified_library_backup(
    store: &mut PersistentStore,
    revision: i64,
    destination: &Path,
    scratch: &Path,
    probe: &dyn CancellationProbe,
) -> Result<String> {
    let mut pins = DurableCasJob::begin(
        store.repository_root(),
        &uuid::Uuid::new_v4().to_string(),
        CasJobKind::OfficialPublicationOrExportPreparation,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .try_into()
            .unwrap_or(i64::MAX),
    )?;
    let outcome = (|| {
        let captured = capture_library(store, revision, scratch, &mut pins, false, probe)?;
        if captured.repair_required {
            return Err(Error::Invalid("internal backup requires a valid library"));
        }
        captured
            .catalog
            .write_candidate(destination, false, probe)?;
        let archive = VerifiedArchive::open(File::open(destination)?, scratch, probe)?;
        archive.validate_library(probe)?;
        if !archive.manifest.library_included || archive.manifest.device_included {
            return Err(Error::Invalid("internal backup scope differs"));
        }
        drop(archive);
        let mut file = File::open(destination)?;
        let bytes = file.metadata()?.len();
        copy_hash(&mut file, &mut io::sink(), bytes, probe)
    })();
    let released = pins.release(CasReleaseOutcome::Aborted);
    match (outcome, released) {
        (Ok(hash), Ok(())) => Ok(hash),
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error.into()),
    }
}
