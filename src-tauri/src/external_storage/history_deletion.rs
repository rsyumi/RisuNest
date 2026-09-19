//! Authenticated, point-level deletion for ordinary retained backup history.
use super::{
    connection_commands::{self, ConnectedRepository},
    contract::{Cancellation, ErrorKind, LeaseKind, ProviderError, Result},
    control::{self, BackupPointKind, ListedBackupPoint, RemoteBackupPointDeleteOutcome},
    job_store::DurableJob,
    leases::{self, Admission, PageTracker},
    runtime,
};
use risunest_external_storage_format::snapshot::StoredObject;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrepareHistoryDeleteRequest {
    connection_id: String,
    point_id: String,
    point_observation: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct HistoryDeletePreparation {
    same_device: bool,
    last_retained: bool,
}

struct Inspection {
    point: ListedBackupPoint,
    same_device: bool,
    last_retained: bool,
}

async fn list_points(connected: &ConnectedRepository, cancel: &Cancellation) -> Result<Vec<ListedBackupPoint>> {
    let mut points = Vec::new();
    let mut cursor = None;
    let mut tracker = PageTracker::default();
    let mut ids = std::collections::BTreeSet::new();
    loop {
        let page = leases::control_request(cancel, control::list_connected_backup_points_page(
            connected, cursor.as_deref(), 100, cancel,
        )).await?;
        let receipts = page.points.iter().map(|point| point.reference.receipt.clone()).collect::<Vec<_>>();
        tracker.accept(&connected.handle, &receipts, page.next_cursor.as_deref())?;
        for point in &page.points {
            if !ids.insert(point.document.point_id.clone()) {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
        }
        points.extend(page.points);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(points),
        }
    }
}

async fn inspect(
    connected: &ConnectedRepository,
    point_id: &str,
    expected: &StoredObject,
    store_id: &str,
    cancel: &Cancellation,
) -> Result<Option<Inspection>> {
    if point_id.is_empty() || point_id.len() > 1024 || point_id.contains('\0')
        || expected.header.repository_id != connected.stored.descriptor.repository_id
        || expected.header.object_id != format!("backup-point-{point_id}")
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let points = list_points(connected, cancel).await?;
    let retained = points.iter().filter(|point| matches!(
        point.document.kind, BackupPointKind::Automatic | BackupPointKind::Manual
    )).count();
    let Some(point) = points.into_iter().find(|point| point.document.point_id == point_id) else {
        return Ok(None);
    };
    if !matches!(point.document.kind, BackupPointKind::Automatic | BackupPointKind::Manual)
        || point.reference.stored(&connected.handle)? != *expected
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let bundle = control::read_snapshot_document(connected, &point.document.bundle, cancel).await?;
    Ok(Some(Inspection {
        same_device: !store_id.is_empty() && bundle.captured_by_device.as_deref() == Some(store_id),
        last_retained: retained == 1,
        point,
    }))
}

#[tauri::command]
pub(crate) async fn external_storage_prepare_history_delete(
    app: tauri::AppHandle,
    request: PrepareHistoryDeleteRequest,
) -> Result<HistoryDeletePreparation> {
    let connected = connection_commands::open_connected(&app, &request.connection_id).await?;
    let observation: StoredObject = serde_json::from_str(&request.point_observation)
        .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    let store_id = runtime::native_store(&app)?.external_identity().map_err(runtime::local_error)?.store_id;
    let inspected = inspect(&connected, &request.point_id, &observation, &store_id, &Cancellation::default())
        .await?
        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
    Ok(HistoryDeletePreparation {
        same_device: inspected.same_device,
        last_retained: inspected.last_retained,
    })
}

pub(crate) async fn run_delete_history(
    app: &tauri::AppHandle,
    connected: &ConnectedRepository,
    job: &DurableJob,
    cancel: &Cancellation,
) -> Result<Value> {
    if !connected.stored.capabilities.cleanup_supported() {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let point_id = job.request.point_id.as_deref().ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
    let expected: StoredObject = serde_json::from_str(
        job.request.point_observation.as_deref().ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?
    ).map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    let root = runtime::root(app)?;
    let writer_id = runtime::native_store(app)?.external_identity().map_err(runtime::local_error)?.store_id;
    let context = leases::LeaseContext {
        root: &root,
        connection_id: &connected.stored.id,
        writer_id: &writer_id,
        descriptor: &connected.stored.descriptor,
        root_key: &connected.root_key,
        provider: connected.provider.as_ref(),
        repository: &connected.handle,
        clock: leases::system_clock(),
        protection_supported: connected.stored.capabilities.lease_operations,
    };
    let owner = match leases::admit(&context, &job.id, LeaseKind::Cleanup, cancel).await? {
        Admission::Admitted(owner) => owner,
        Admission::Yield { .. } => return Err(ProviderError::new(ErrorKind::Transient)),
        Admission::UnsupportedProtection => return Err(ProviderError::new(ErrorKind::Unsupported)),
    };
    owner.run(&context, cancel, async {
        let Some(inspected) = inspect(connected, point_id, &expected, &writer_id, cancel).await? else {
            return Ok(json!({"pointId":point_id,"deleteOutcome":"not-found"}));
        };
        if (!inspected.same_device && job.request.confirm_other_device != Some(true))
            || (inspected.last_retained && job.request.confirm_last_retained != Some(true))
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        owner.place_marker(&context, cancel).await?;
        if owner.recheck(&context, cancel).await?.is_some() {
            return Err(ProviderError::new(ErrorKind::Transient));
        }
        let stored = inspected.point.reference.stored(&connected.handle)?;
        owner.check_control(&context, true)?;
        owner.set_delete_in_flight(true);
        let outcome = leases::control_request(cancel, control::delete_authenticated_backup_point(
            connected, point_id, inspected.point.document.kind, &stored, cancel,
        )).await?;
        owner.set_delete_in_flight(false);
        Ok(json!({
            "pointId": point_id,
            "deleteOutcome": match outcome {
                RemoteBackupPointDeleteOutcome::Deleted => "deleted",
                RemoteBackupPointDeleteOutcome::NotFound => "not-found",
            }
        }))
    }).await
}
