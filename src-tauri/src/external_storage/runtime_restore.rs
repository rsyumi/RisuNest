//! Manual external snapshot restore and durable commit recovery.
//! Remote reads finish before either the library writer fence or PDS is opened.
use super::{
    connection_commands::ConnectedRepository,
    connection_store::ConnectionStore,
    contract::{Cancellation, ErrorKind, ProviderError, Result},
    job_store::{DurableJob, JobKind, JobStore},
    runtime,
    snapshot_restore::{self, PreparedRemoteSnapshot},
};
use crate::persistent_store::{
    commands::PersistentStoreState,
    external_apply::{ExternalSnapshotApplication, ExternalSnapshotObject, ExternalSnapshotRecord},
    PersistentStore, StoreError,
};
use risunest_external_storage_format::section::SectionKind;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeSet, path::Path};
use tauri::{AppHandle, Manager};

#[derive(Clone, Debug, PartialEq, Eq)]
struct RestoreSelection {
    library: bool,
    sections: BTreeSet<String>,
}

const RESTORE_COMMIT_SCHEMA: &str = "risunest.external-restore-commit/v1";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RestoreCommitMarker {
    schema: String,
    job_id: String,
    connection_id: String,
    snapshot_id: String,
    expected_revision: String,
    received_revision: String,
}

fn restore_marker_key(job: &str) -> String {
    format!("external-restore-commit:{job}")
}

fn restore_marker(job: &DurableJob, expected_revision: i64) -> Result<RestoreCommitMarker> {
    let received_revision = expected_revision
        .checked_add(1)
        .ok_or_else(|| ProviderError::new(ErrorKind::PreconditionFailed))?;
    Ok(RestoreCommitMarker {
        schema: RESTORE_COMMIT_SCHEMA.into(),
        job_id: job.id.clone(),
        connection_id: job.request.connection_id.clone(),
        snapshot_id: job.request.snapshot_id.clone().ok_or_else(corrupt)?,
        expected_revision: expected_revision.to_string(),
        received_revision: received_revision.to_string(),
    })
}

fn completed_restore_in_store(store: &PersistentStore, job: &DurableJob) -> Result<Option<Value>> {
    if job.request.kind != JobKind::Restore {
        return Ok(None);
    }
    let expected_revision = job
        .request
        .target_revision
        .as_deref()
        .ok_or_else(corrupt)?
        .parse::<i64>()
        .map_err(|_| corrupt())?;
    let Some(value) = store
        .get_app_kv(&restore_marker_key(&job.id))
        .map_err(pds_error)?
    else {
        return Ok(None);
    };
    let marker: RestoreCommitMarker = serde_json::from_value(value).map_err(|_| corrupt())?;
    let expected = restore_marker(job, expected_revision)?;
    if marker != expected
        || store.revision().map_err(pds_error)?
            < marker
                .received_revision
                .parse::<i64>()
                .map_err(|_| corrupt())?
    {
        return Err(corrupt());
    }
    Ok(Some(json!({
        "snapshotId":marker.snapshot_id,
        "receivedRevision":marker.received_revision
    })))
}

/// Read-only completion recovery used before reopening a provider connection.
pub(crate) fn completed_restore(app: &AppHandle, job: &DurableJob) -> Result<Option<Value>> {
    completed_restore_in_store(&runtime::native_store(app)?, job)
}

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

fn pds_error(error: StoreError) -> ProviderError {
    match error {
        StoreError::RevisionConflict { .. } => ProviderError::new(ErrorKind::PreconditionFailed),
        StoreError::Validation { .. } => corrupt(),
        _ => ProviderError::new(ErrorKind::Transient),
    }
}

fn decode_hash(value: &str) -> Result<[u8; 32]> {
    if !crate::trust_boundary::is_lower_hex_256(value) {
        return Err(corrupt());
    }
    hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(corrupt)
}

/// A bundle declares what it covers, and a restore asks for a subset of that.
/// An area the request does not name is left alone on this device, and an
/// unknown one is rejected rather than quietly narrowed.
fn restore_selection(restore_areas: Option<&[String]>) -> Result<RestoreSelection> {
    let defaults;
    let areas = match restore_areas {
        Some(areas) => areas,
        None => {
            defaults = vec!["library".to_owned(), "referencedAssets".to_owned()];
            &defaults
        }
    };
    let unique = areas.iter().collect::<BTreeSet<_>>();
    if unique.len() != areas.len() {
        return Err(corrupt());
    }
    let library_areas = ["library", "referencedAssets"];
    let mut sections = BTreeSet::new();
    for area in areas {
        if library_areas.contains(&area.as_str()) {
            continue;
        }
        sections.insert(SectionKind::parse(area).map_err(|_| corrupt())?.id().to_owned());
    }
    let library = areas
        .iter()
        .any(|area| library_areas.contains(&area.as_str()));
    if !library {
        return Err(corrupt());
    }
    Ok(RestoreSelection { library, sections })
}

/// Local settings belong to the device that wrote them, so only that device's
/// own backup may bring them back. Published states and another device's
/// bundles are refused here rather than at the screen, because the request
/// names the areas and the screen is not the only caller.
fn require_restorable_sections(
    selection: &RestoreSelection,
    snapshot: &PreparedRemoteSnapshot,
    store_id: &str,
) -> Result<()> {
    if !selection.sections.contains(SectionKind::LocalSettings.id()) {
        return Ok(());
    }
    if store_id.is_empty() || snapshot.captured_by_device.as_deref() != Some(store_id) {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(())
}

fn checked_required_bytes(
    snapshot: &PreparedRemoteSnapshot,
    selection: &RestoreSelection,
) -> Result<u64> {
    let mut total = 0u64;
    let mut add = |length: u64| -> Result<()> {
        total = total.checked_add(length).ok_or_else(corrupt)?;
        Ok(())
    };
    if selection.library {
        for record in &snapshot.records {
            add(record.byte_length)?;
        }
        for object in &snapshot.objects {
            add(object.byte_length)?;
        }
    }
    Ok(total)
}

#[cfg(windows)]
fn available_space(path: &Path) -> std::io::Result<u64> {
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide.push(0);
    let mut available = 0u64;
    let result = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(available)
    }
}

#[cfg(unix)]
fn available_space(path: &Path) -> std::io::Result<u64> {
    use std::{ffi::CString, mem::MaybeUninit, os::unix::ffi::OsStrExt};
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid path"))?;
    let mut stats = MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::statvfs(path.as_ptr(), stats.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

fn validate_download(
    connected: &ConnectedRepository,
    requested_snapshot: &str,
    snapshot: &PreparedRemoteSnapshot,
) -> Result<()> {
    if snapshot.snapshot_id != requested_snapshot
        || snapshot.repository_id != connected.stored.descriptor.repository_id
    {
        return Err(corrupt());
    }
    decode_hash(&snapshot.fingerprint)?;
    decode_hash(&snapshot.library_fingerprint)?;
    Ok(())
}

fn update_phase(root: &Path, job: &DurableJob, phase: &str) -> Result<()> {
    let store = JobStore::open(root)?;
    let mut current = store.read(&job.id)?;
    if current.request.connection_id != job.request.connection_id || current.terminal() {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    current.summary["state"] = json!("running");
    current.summary["phase"] = json!(phase);
    current.summary["updatedAtMs"] = json!(runtime::now_ms().to_string());
    store.put(&current)
}

pub(crate) async fn run_restore(
    app: &AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    cancel: &Cancellation,
) -> Result<Value> {
    if job.request.kind != JobKind::Restore {
        return Err(corrupt());
    }
    if let Some(result) = completed_restore(app, job)? {
        return Ok(result);
    }
    let snapshot_id = job.request.snapshot_id.as_deref().ok_or_else(corrupt)?;
    let expected_revision = job
        .request
        .target_revision
        .as_deref()
        .ok_or_else(corrupt)?
        .parse::<i64>()
        .map_err(|_| corrupt())?;
    let root = runtime::root(app)?;
    update_phase(&root, job, "downloading")?;
    let known = match ConnectionStore::open(&root)?
        .discovery_snapshot(&job.request.connection_id, snapshot_id)
    {
        Ok(value) => Some(value),
        Err(error) if error.kind == ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let cache_root = root.clone();
    let cache_connection = job.request.connection_id.clone();
    let cache_id = snapshot_id.to_owned();
    let remote = super::control::find_snapshot_with_locator_invalidation(
        connected,
        snapshot_id,
        known.as_ref(),
        move || {
            ConnectionStore::open(&cache_root)?.forget_discovery(&cache_connection, &cache_id)
        },
        cancel,
    )
    .await?;
    ConnectionStore::open(&root)?.remember_discovery(
        &job.request.connection_id,
        snapshot_id,
        &remote,
    )?;
    let staging_root =
        runtime::job_directory(&root, &job.request.connection_id, &job.id).join("restore-snapshot");
    let snapshot = snapshot_restore::download_snapshot(
        &remote,
        &staging_root,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    cancel.check()?;
    validate_download(connected, snapshot_id, &snapshot)?;
    let selection = restore_selection(job.request.restore_areas.as_deref())?;
    require_restorable_sections(&selection, &snapshot, &job.admission_identity.store_id)?;
    let sections = snapshot_restore::download_sections(
        &remote,
        &selection.sections,
        &staging_root,
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        cancel,
    )
    .await?;
    if sections.len() != selection.sections.len() {
        return Err(ProviderError::new(ErrorKind::NotFound));
    }
    let required = checked_required_bytes(&snapshot, &selection)?;
    if available_space(&staging_root).map_err(runtime::local_error)? < required {
        return Err(ProviderError::new(ErrorKind::StorageFull));
    }
    update_phase(&root, job, "preparing-local")?;

    let worker_app = app.clone();
    let worker_job = job.clone();
    let worker_cancel = cancel.clone();
    let result = tokio::task::spawn_blocking(move || {
        prepare_local_restore(
            &worker_app,
            &worker_job,
            expected_revision,
            snapshot,
            selection,
            sections,
            worker_cancel,
        )
    })
    .await
    .map_err(runtime::local_error)??;
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
fn prepare_local_restore(
    app: &AppHandle,
    job: &DurableJob,
    expected_revision: i64,
    snapshot: PreparedRemoteSnapshot,
    selection: RestoreSelection,
    sections: Vec<super::sections::CapturedSection>,
    cancel: Cancellation,
) -> Result<Value> {
    cancel.check()?;
    let admission = app
        .state::<crate::native_file_jobs::NativeFileJobState>()
        .admission
        .clone();
    let permit = admission.file(true).map_err(runtime::local_error)?;
    // Open a dedicated native connection while renderer admission is still
    // available. It remains owned across the maintenance WebView reload.
    let mut store = runtime::native_store(app)?;
    let maintenance = app
        .state::<PersistentStoreState>()
        .acquire_device_maintenance()
        .map_err(pds_error)?;
    cancel.check()?;
    if store.revision().map_err(pds_error)? != expected_revision {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let snapshot_id = snapshot.snapshot_id.clone();
    let staging_root = snapshot.staging_root.clone();
    let prepared = if selection.library {
        let scope_id = risunest_external_storage_format::format::library_fingerprint_domain();
        let fingerprint = decode_hash(&snapshot.library_fingerprint)?;
        let records = snapshot.records.into_iter().map(|record| {
            Ok(ExternalSnapshotRecord {
                key: record.key,
                content_hash: record.content_hash,
                byte_length: record.byte_length,
                path: record.path,
            })
        });
        let objects = snapshot.objects.into_iter().map(|object| {
            Ok(ExternalSnapshotObject {
                content_hash: object.content_hash,
                byte_length: object.byte_length,
                path: object.path,
            })
        });
        Some(
            store
                .prepare_external_snapshot_application(
                    &ExternalSnapshotApplication {
                        expected_revision,
                        staging_root: &staging_root,
                        scope_id: &scope_id,
                        fingerprint: &fingerprint,
                    },
                    records,
                    objects,
                )
                .map_err(pds_error)?,
        )
    } else {
        None
    };

    let prepared = prepared.ok_or_else(corrupt)?;
    // Preparing every selected section before touching the device file keeps a
    // bundle with a missing object from installing half of itself.
    let prepared_sections =
        super::sections::prepare_received_backup_sections(&sections, &cancel)?;
    let device = store.device_store_mut().map_err(pds_error)?;
    for rows in &prepared_sections {
        cancel.check()?;
        device.restore_prepared_backup_section(rows).map_err(pds_error)?;
    }
    let marker = restore_marker(job, expected_revision)?;
    let revision = store
        .finish_prepared_replace_with_app_kv(
            prepared,
            &restore_marker_key(&job.id),
            &serde_json::to_value(&marker).map_err(runtime::local_error)?,
        )
        .map_err(pds_error)?;
    if revision.revision.to_string() != marker.received_revision {
        return Err(corrupt());
    }
    drop(maintenance);
    drop(permit);
    cleanup_staging(&staging_root);
    Ok(json!({
        "snapshotId": snapshot_id,
        "receivedRevision": revision.revision.to_string()
    }))
}

fn cleanup_staging(path: &Path) {
    let safe = std::fs::symlink_metadata(path)
        .ok()
        .is_some_and(|metadata| {
            metadata.is_dir() && !crate::trust_boundary::is_link_like(&metadata)
        });
    if safe {
        let _ = std::fs::remove_dir_all(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistent_store::sync_selection::CaptureIdentity;

    fn identity() -> CaptureIdentity {
        CaptureIdentity {
            store_id: "store".into(),
            library_epoch: "library".into(),
            generation: "generation".into(),
            selection_epoch: "selection".into(),
            revision: 0,
        }
    }

    #[test]
    fn restore_area_selection_matches_the_renderer_contract() {
        assert_eq!(
            restore_selection(None).unwrap(),
            RestoreSelection {
                library: true,
                sections: BTreeSet::new()
            }
        );
        assert_eq!(
            restore_selection(Some(&["referencedAssets".into()])).unwrap(),
            RestoreSelection {
                library: true,
                sections: BTreeSet::new()
            }
        );
        assert_eq!(
            restore_selection(Some(&["library".into(), "hypa".into(), "local-settings".into()]))
                .unwrap(),
            RestoreSelection {
                library: true,
                sections: BTreeSet::from(["hypa".to_owned(), "local-settings".to_owned()])
            }
        );
    }

    /// A bundle declares what it covers. Asking for coverage it does not
    /// declare fails rather than restoring a narrowed selection.
    #[test]
    fn restore_area_selection_rejects_duplicates_and_undeclared_areas() {
        assert!(restore_selection(Some(&["library".into(), "library".into()])).is_err());
        assert!(restore_selection(Some(&["devicePlugins".into()])).is_err());
        assert!(restore_selection(Some(&["deviceSettings".into()])).is_err());
        assert!(restore_selection(Some(&["hypa".into()])).is_err());
        assert!(restore_selection(Some(&[])).is_err());
    }

    fn prepared(captured_by_device: Option<&str>) -> PreparedRemoteSnapshot {
        PreparedRemoteSnapshot {
            snapshot_id: "synthetic-snapshot".into(),
            repository_id: "synthetic-repository".into(),
            fingerprint: "00".repeat(32),
            library_fingerprint: "00".repeat(32),
            logical_revision: 1,
            staging_root: std::path::PathBuf::from("staging"),
            records: Vec::new(),
            objects: Vec::new(),
            captured_by_device: captured_by_device.map(ToOwned::to_owned),
        }
    }

    /// Invariant 32 on the restore side. Local settings belong to the device
    /// that wrote them, so only that device's own backup installs them. Another
    /// device's backup and a published state are refused even when the request
    /// names the area, and the areas beside them are unaffected.
    #[test]
    fn another_devices_backup_does_not_install_local_settings() {
        let areas = [
            "library".to_owned(),
            "hypa".to_owned(),
            "local-settings".to_owned(),
        ];
        let selection = restore_selection(Some(&areas)).unwrap();
        assert!(require_restorable_sections(&selection, &prepared(Some("this-device")), "this-device").is_ok());
        assert!(require_restorable_sections(
            &selection,
            &prepared(Some("other-device")),
            "this-device"
        )
        .is_err());
        assert!(require_restorable_sections(&selection, &prepared(None), "this-device").is_err());
        assert!(require_restorable_sections(&selection, &prepared(Some("")), "").is_err());

        let without = ["library".to_owned(), "hypa".to_owned(), "local-plugins".to_owned()];
        let selection = restore_selection(Some(&without)).unwrap();
        assert!(require_restorable_sections(&selection, &prepared(None), "this-device").is_ok());
        assert!(require_restorable_sections(
            &selection,
            &prepared(Some("other-device")),
            "this-device"
        )
        .is_ok());
    }

    #[test]
    fn library_restore_marker_recovers_only_the_exact_durable_request() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(root.path()).unwrap();
        let request = serde_json::from_value(json!({
            "connectionId":"synthetic-connection",
            "kind":"restore",
            "snapshotId":"synthetic-snapshot",
            "targetRevision":"0"
        }))
        .unwrap();
        let job = DurableJob::new(request, false, 1, identity());
        assert!(completed_restore_in_store(&store, &job).unwrap().is_none());
        let marker = restore_marker(&job, 0).unwrap();
        store
            .set_app_kv(
                &restore_marker_key(&job.id),
                &serde_json::to_value(marker).unwrap(),
            )
            .unwrap();
        let database =
            rusqlite::Connection::open(root.path().join("persistent/persistent.sqlite")).unwrap();
        database
            .execute("UPDATE meta SET value='1' WHERE key='currentRevision'", [])
            .unwrap();

        assert_eq!(
            completed_restore_in_store(&store, &job).unwrap().unwrap(),
            json!({"snapshotId":"synthetic-snapshot","receivedRevision":"1"})
        );
        let mut different = job.clone();
        different.request.snapshot_id = Some("different-snapshot".into());
        assert!(completed_restore_in_store(&store, &different).is_err());
    }
}
