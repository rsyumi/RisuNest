//! User-requested discovery of authenticated history roots, including orphan snapshots.
use super::{
    connection_commands::ConnectedRepository, connection_store::ConnectionStore, contract::*,
    control, packaging::{self, RemoteObject}, runtime, transfer::SpoolSink,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use risunest_external_storage_format::{
    content_identity::hash, crypto::derive_key, snapshot as wire,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, io::Cursor};
use tauri::{AppHandle, Manager};

const PAGE: u16 = 30;
const MAX_CIPHERTEXT: u64 = 16 * 1024 * 1024;
fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CursorState {
    connection: String,
    phase: String,
    provider: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct HistoryRequest {
    connection_id: String,
    cursor: Option<String>,
}
fn decode_cursor(request: &HistoryRequest) -> Result<CursorState> {
    let Some(value) = &request.cursor else {
        return Ok(CursorState {
            connection: request.connection_id.clone(),
            phase: "points".into(),
            provider: None,
        });
    };
    if value.len() > 8192 {
        return Err(corrupt());
    }
    let bytes = URL_SAFE_NO_PAD.decode(value).map_err(|_| corrupt())?;
    let state: CursorState = serde_json::from_slice(&bytes).map_err(|_| corrupt())?;
    if state.connection != request.connection_id
        || !["points", "snapshots"].contains(&state.phase.as_str())
        || state
            .provider
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.len() > 4096)
    {
        return Err(corrupt());
    }
    Ok(state)
}
fn encode_cursor(state: CursorState) -> Result<String> {
    Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&state).map_err(runtime::local_error)?))
}
async fn open_snapshot(
    app: &AppHandle,
    connected: &ConnectedRepository,
    receipt: ObjectReceipt,
    cancel: &Cancellation,
) -> Result<(RemoteObject, control::SnapshotView)> {
    receipt.locator.validate_for(&connected.handle)?;
    if !receipt.complete || receipt.byte_length == 0 || receipt.byte_length > MAX_CIPHERTEXT {
        return Err(corrupt());
    }
    let directory = tempfile::tempdir_in(runtime::root(app)?).map_err(runtime::local_error)?;
    let path = directory.path().join("snapshot");
    let mut sink = SpoolSink::create(&path, receipt.byte_length)?;
    if !matches!(
        connected
            .provider
            .read_object(&connected.handle, &receipt.locator, None, &mut sink, cancel)
            .await?,
        ReadReceipt::Body(_)
    ) || !sink.is_verified()
    {
        return Err(corrupt());
    }
    let bytes = std::fs::read(&path).map_err(runtime::local_error)?;
    if bytes.len() as u64 != receipt.byte_length {
        return Err(corrupt());
    }
    let key = derive_key(
        &connected.root_key,
        &connected.stored.descriptor.repository_id,
        "metadata",
    )
    .map_err(|_| corrupt())?;
    let mut plaintext = Vec::new();
    let header = wire::open_envelope(
        &mut Cursor::new(&bytes),
        &mut plaintext,
        &key,
        wire::MAX_METADATA_BYTES as u64,
    )
    .map_err(|_| corrupt())?;
    if header.repository_id != connected.stored.descriptor.repository_id {
        return Err(corrupt());
    }
    let role = packaging::native_role(header.role).map_err(|_| corrupt())?;
    let document = control::SnapshotView::read(
        &plaintext,
        header.role,
        &connected.stored.descriptor.repository_id,
    )
    .map_err(|_| corrupt())?;
    if header.object_id != format!("snapshot-{}", document.snapshot_id) {
        return Err(corrupt());
    }
    Ok((
        RemoteObject {
            repository_id: header.repository_id,
            object_id: header.object_id,
            role,
            receipt,
            ciphertext_sha256: hex::encode(hash(&bytes)),
            plaintext_length: header.plaintext_length,
            plaintext_sha256: hex::encode(hash(&plaintext)),
        },
        document,
    ))
}
/// `includedSections` is what a restore may choose from, and `sameDevice` says
/// whether the values are this device's own. Both come from the authenticated
/// document, so the screen never guesses the coverage from the connection.
fn item(
    document: &control::SnapshotView,
    reference: &RemoteObject,
    kind: &str,
    pinned: bool,
    store_id: &str,
) -> Value {
    let same_device =
        !store_id.is_empty() && document.captured_by_device.as_deref() == Some(store_id);
    json!({"id":document.snapshot_id,"kind":kind,"createdAtMs":document.created_at_ms.to_string(),"logicalRevision":document.revision,"storedBytes":reference.receipt.byte_length.to_string(),"pinned":pinned,"complete":true,"verified":true,
        "includedSections":document.sections.keys().collect::<Vec<_>>(),"sameDevice":same_device,
        "warning":"Snapshot metadata is authenticated. All referenced data is verified before restore."})
}
fn remember_item(items: &mut BTreeMap<String, Value>, id: String, mut next: Value) {
    if let Some(previous) = items.get(&id) {
        next["pinned"] = json!(previous["pinned"] == true || next["pinned"] == true);
        let rank = |value: &Value| match value.as_str() {
            Some("conflict") => 2,
            Some("backup-point") => 1,
            _ => 0,
        };
        if rank(&previous["kind"]) > rank(&next["kind"]) {
            next["kind"] = previous["kind"].clone();
        }
    }
    items.insert(id, next);
}
#[tauri::command]
pub(crate) async fn external_storage_list_history(
    app: AppHandle,
    request: HistoryRequest,
) -> Result<Value> {
    let state = decode_cursor(&request)?;
    let connected =
        super::connection_commands::open_connected(&app, &request.connection_id).await?;
    let cancel = Cancellation::default();
    let cache = ConnectionStore::open(&runtime::root(&app)?)?;
    let store_id = crate::persistent_store::commands::with_store_mut(app.state(), |store| {
        store.external_identity()
    })
    .map_err(runtime::local_error)?
    .store_id;
    let mut items = BTreeMap::new();
    let next = if state.phase == "points" {
        let page = control::list_connected_backup_points_page(
            &connected,
            state.provider.as_deref(),
            PAGE,
            &cancel,
        )
        .await?;
        for point in page.points {
            let kind = match point.document.kind {
                control::BackupPointKind::Conflict => "conflict",
                control::BackupPointKind::RecoveryCandidate => "recovery-candidate",
                _ => "backup-point",
            };
            let pinned = point.document.kind == control::BackupPointKind::Manual;
            for reference in point.document.bundles().into_iter().cloned() {
                let document =
                    control::read_snapshot_document(&connected, &reference, &cancel).await?;
                cache.remember_discovery(
                    &request.connection_id,
                    &document.snapshot_id,
                    &reference,
                )?;
                remember_item(
                    &mut items,
                    document.snapshot_id.clone(),
                    item(&document, &reference, kind, pinned, &store_id),
                );
            }
        }
        match page.next_cursor {
            Some(provider) => Some(CursorState {
                connection: request.connection_id.clone(),
                phase: "points".into(),
                provider: Some(provider),
            }),
            None => Some(CursorState {
                connection: request.connection_id.clone(),
                phase: "snapshots".into(),
                provider: None,
            }),
        }
    } else {
        let page = connected
            .provider
            .list_objects(
                &connected.handle,
                Collection::Snapshots,
                state.provider.as_deref(),
                PAGE,
                &cancel,
            )
            .await?;
        for receipt in page.objects {
            let (reference, document) = open_snapshot(&app, &connected, receipt, &cancel).await?;
            cache.remember_discovery(&request.connection_id, &document.snapshot_id, &reference)?;
            remember_item(
                &mut items,
                document.snapshot_id.clone(),
                item(&document, &reference, "recovery-candidate", false, &store_id),
            );
        }
        page.next_cursor.map(|provider| CursorState {
            connection: request.connection_id.clone(),
            phase: "snapshots".into(),
            provider: Some(provider),
        })
    };
    let mut output = json!({"items":items.into_values().collect::<Vec<_>>()});
    if let Some(next) = next {
        output["nextCursor"] = json!(encode_cursor(next)?);
    }
    Ok(output)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duplicate_snapshot_keeps_pinning_and_conflict_evidence() {
        let mut items = BTreeMap::new();
        remember_item(
            &mut items,
            "snapshot".into(),
            json!({"kind":"backup-point","pinned":true}),
        );
        remember_item(
            &mut items,
            "snapshot".into(),
            json!({"kind":"conflict","pinned":false}),
        );
        remember_item(
            &mut items,
            "snapshot".into(),
            json!({"kind":"recovery-candidate","pinned":false}),
        );
        assert_eq!(items["snapshot"]["kind"], "conflict");
        assert_eq!(items["snapshot"]["pinned"], true);
    }
    #[test]
    fn cursor_cannot_cross_connections_or_select_arbitrary_collections() {
        let encoded = encode_cursor(CursorState {
            connection: "one".into(),
            phase: "snapshots".into(),
            provider: Some("opaque".into()),
        })
        .unwrap();
        assert!(decode_cursor(&HistoryRequest {
            connection_id: "two".into(),
            cursor: Some(encoded.clone())
        })
        .is_err());
        assert_eq!(
            decode_cursor(&HistoryRequest {
                connection_id: "one".into(),
                cursor: Some(encoded)
            })
            .unwrap()
            .provider
            .as_deref(),
            Some("opaque")
        );
        let encoded = encode_cursor(CursorState {
            connection: "one".into(),
            phase: "packs".into(),
            provider: None,
        })
        .unwrap();
        assert!(decode_cursor(&HistoryRequest {
            connection_id: "one".into(),
            cursor: Some(encoded)
        })
        .is_err());
    }
}
