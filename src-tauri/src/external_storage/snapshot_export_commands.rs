//! User-selected standalone archive export for authenticated history entries.
//! Remote bytes are fully downloaded and verified before the archive is built.
use super::{
    connection_commands,
    contract::{Cancellation, ErrorKind, ProviderError, Result},
    control, snapshot_export, snapshot_restore,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
};
use tauri::AppHandle;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_fs::{FilePath, FsExt, OpenOptions};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExportSnapshotRequest {
    connection_id: String,
    snapshot_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportSnapshotResponse {
    cancelled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    destination: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 1024 && !value.contains('\0')
}

fn archive_name(snapshot_id: &str) -> String {
    let safe: String = snapshot_id
        .chars()
        .filter(|value| value.is_ascii_alphanumeric() || matches!(value, '-' | '_'))
        .take(64)
        .collect();
    format!(
        "RisuNest-{}.risunest",
        if safe.is_empty() { "snapshot" } else { &safe }
    )
}

fn selected_path(app: &AppHandle, snapshot_id: &str) -> Option<FilePath> {
    app.dialog()
        .file()
        .add_filter("RisuNest backup", &["risunest"])
        .set_file_name(archive_name(snapshot_id))
        .blocking_save_file()
}

fn publish_uri_destination(
    app: &AppHandle,
    selected: FilePath,
    candidate: &std::path::Path,
    expected_hash: &str,
    cancel: &Cancellation,
) -> Result<()> {
    let mut source = File::open(candidate).map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    let mut output = app
        .fs()
        .open(selected.clone(), options)
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        cancel.check()?;
        let count = source
            .read(&mut buffer)
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    }
    output
        .flush()
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    output
        .sync_all()
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    drop(output);
    let mut options = OpenOptions::new();
    options.read(true);
    let mut output = app
        .fs()
        .open(selected, options)
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let mut digest = Sha256::new();
    loop {
        cancel.check()?;
        let count = output
            .read(&mut buffer)
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    if hex::encode(digest.finalize()) != expected_hash {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(())
}

#[tauri::command(async)]
pub(crate) async fn external_storage_export_snapshot(
    app: AppHandle,
    request: ExportSnapshotRequest,
) -> Result<ExportSnapshotResponse> {
    if !valid_id(&request.connection_id) || !valid_id(&request.snapshot_id) {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let Some(selected) = selected_path(&app, &request.snapshot_id) else {
        return Ok(ExportSnapshotResponse {
            cancelled: true,
            destination: None,
            sha256: None,
        });
    };
    let root = super::runtime::root(&app)?;
    std::fs::create_dir_all(root.join("external-storage"))
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let staging = tempfile::Builder::new()
        .prefix("external-snapshot-download-")
        .tempdir_in(root.join("external-storage"))
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let cancel = Cancellation::default();
    let connected = connection_commands::open_connected(&app, &request.connection_id).await?;
    let remote = control::find_snapshot(&connected, &request.snapshot_id, &cancel).await?;
    let prepared = snapshot_restore::download_snapshot(
        &remote,
        &staging.path().join("verified"),
        &connected.root_key,
        connected.provider.as_ref(),
        &connected.handle,
        &cancel,
    )
    .await?;
    if prepared.snapshot_id != request.snapshot_id {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let path_destination = selected.clone().into_path().ok();
    let local_destination = path_destination
        .clone()
        .unwrap_or_else(|| staging.path().join("selected-snapshot.risunest"));
    let receipt = snapshot_export::export_verified_snapshot(
        prepared,
        &risunest_external_storage_format::format::Scope::LIBRARY,
        &local_destination,
        &staging.path().join("export"),
        &cancel,
    )?;
    if path_destination.is_none() {
        publish_uri_destination(
            &app,
            selected.clone(),
            &local_destination,
            &receipt.sha256,
            &cancel,
        )?;
    }
    Ok(ExportSnapshotResponse {
        cancelled: false,
        destination: Some(selected.to_string()),
        sha256: Some(receipt.sha256),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_bounded_before_dialog_or_network() {
        assert!(valid_id("connection"));
        assert!(!valid_id(""));
        assert!(!valid_id(&"x".repeat(1025)));
        assert!(!valid_id("bad\0id"));
        assert_eq!(
            archive_name("../snapshot/id"),
            "RisuNest-snapshotid.risunest"
        );
    }
}
