//! Raw commit payload shared by Windows and Linux. Store/revision semantics stay in PDS.
use crate::persistent_store::{
    commands::with_store_mut, AssetAlias, RevisionResult, StoreError, StoreResult, WorkingSetCommit,
};
use serde::Deserialize;
use tauri::{
    ipc::{InvokeBody, Request},
    AppHandle, Manager, WebviewWindow,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Envelope {
    commit: WorkingSetCommit,
    asset_aliases: Vec<AssetAlias>,
}

pub(crate) fn commit_bytes(app: &AppHandle, bytes: &[u8]) -> StoreResult<RevisionResult> {
    let envelope: Envelope = serde_json::from_slice(bytes).map_err(|_| StoreError::Validation {
        message: "invalid commit envelope JSON".to_owned(),
    })?;
    with_store_mut(app.state(), |store| {
        store.commit_with_asset_aliases(&envelope.commit, &envelope.asset_aliases)
    })
}

#[tauri::command(async)]
pub(crate) fn pds_commit_raw(
    app: AppHandle,
    window: WebviewWindow,
    request: Request<'_>,
) -> StoreResult<RevisionResult> {
    if window.label() != "main" {
        return Err(StoreError::Validation {
            message: "commit transport requires the main webview".to_owned(),
        });
    }
    match request.body() {
        InvokeBody::Raw(bytes) => commit_bytes(&app, bytes),
        _ => Err(StoreError::Validation {
            message: "expected a raw commit body".to_owned(),
        }),
    }
}
