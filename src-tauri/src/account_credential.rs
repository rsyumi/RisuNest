//! The account token lives in the OS vault, never in a store file.
//!
//! A snapshot restore replaces whole database files and a backup copies them,
//! so a token kept in the library would travel with the data. The vault
//! implementation is shared with the external storage provider credentials
//! that already use it; only the namespace differs.
use crate::{
    app_data_root,
    external_storage::{auth::SecretBytes, secrets::account_credential_slot},
};
use serde_json::Value;
use tauri::AppHandle;

fn slot(app: &AppHandle) -> Result<crate::external_storage::secrets::NamedSecretSlot, String> {
    let root = app_data_root::resolve(app)
        .map_err(|error| format!("application data root unavailable: {error}"))?;
    Ok(account_credential_slot(&root))
}

#[tauri::command(async)]
pub(crate) fn account_credential_read(app: AppHandle) -> Result<Option<Value>, String> {
    let Some(bytes) = slot(&app)?.read() else {
        return Ok(None);
    };
    match serde_json::from_slice::<Value>(&bytes.0) {
        Ok(value) => Ok(Some(value)),
        Err(_) => {
            crate::nlog!("warn", "stored account credential is not readable");
            Ok(None)
        }
    }
}

#[tauri::command(async)]
pub(crate) fn account_credential_write(app: AppHandle, credential: Value) -> Result<(), String> {
    let serialized =
        serde_json::to_vec(&credential).map_err(|_| "account credential is invalid".to_owned())?;
    slot(&app)?
        .write(&SecretBytes(zeroize::Zeroizing::new(serialized)))
        .map_err(|error| format!("account credential could not be stored: {error}"))
}

#[tauri::command(async)]
pub(crate) fn account_credential_clear(app: AppHandle) -> Result<(), String> {
    slot(&app)?
        .remove()
        .map_err(|error| format!("account credential could not be removed: {error}"))
}
