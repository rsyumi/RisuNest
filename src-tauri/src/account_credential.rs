//! The account token lives in the OS vault, never in a store file.
//!
//! A snapshot restore replaces whole database files and a backup copies them,
//! so a token kept in the library would travel with the data. The vault
//! implementation is shared with the external storage provider credentials
//! that already use it; only the namespace differs.
use crate::{
    app_paths,
    external_storage::{auth::SecretBytes, secrets::account_credential_slot},
};
use serde_json::Value;
use tauri::AppHandle;

/// Keyed by the data root's entry path, so a link at the data root can be
/// added, changed or removed without signing out.
fn slot(app: &AppHandle) -> Result<crate::external_storage::secrets::NamedSecretSlot, String> {
    let root = app_paths::data_entry(app)?;
    Ok(account_credential_slot(&root))
}

#[tauri::command(async)]
pub(crate) fn account_credential_read(app: AppHandle) -> Result<Option<Value>, String> {
    crate::native_log::logged_without_detail("account_credential_read", (|| {
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
    })())
}

pub(crate) fn export_account(app: &AppHandle, omit_account: bool) -> Result<Option<Value>, String> {
    export_account_with(omit_account, || account_credential_read(app.clone()))
}

fn export_account_with(
    omit_account: bool,
    read: impl FnOnce() -> Result<Option<Value>, String>,
) -> Result<Option<Value>, String> {
    if omit_account {
        return Ok(None);
    }
    read()
}

pub(crate) fn validate_account(
    account: &Value,
    expected_id: &str,
    expected_token: &str,
    kei: bool,
) -> Result<(), String> {
    if account.get("id").and_then(Value::as_str) != Some(expected_id)
        || account.get("token").and_then(Value::as_str) != Some(expected_token)
        || expected_id.is_empty()
        || expected_token.is_empty()
        || (kei && account.get("kei").and_then(Value::as_bool) != Some(true))
    {
        return Err("Account credential changed before export".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn omitted_exports_do_not_read_the_vault() {
        assert_eq!(export_account_with(true, || panic!("vault must not be read")).unwrap(), None);
    }

    #[test]
    fn account_including_exports_use_the_fake_vault_and_propagate_failure() {
        let account = json!({"id":"synthetic-account", "token":"synthetic-token", "kei":true});
        assert_eq!(export_account_with(false, || Ok(Some(account.clone()))).unwrap(), Some(account.clone()));
        assert_eq!(export_account_with(false, || Ok(None)).unwrap(), None);
        assert!(export_account_with(false, || Err("synthetic-unavailable".into())).is_err());
        assert!(validate_account(&account, "synthetic-account", "synthetic-token", true).is_ok());
        assert!(validate_account(&account, "foreign-account", "synthetic-token", true).is_err());
        assert!(validate_account(&account, "synthetic-account", "foreign-token", true).is_err());
        assert!(validate_account(&json!({"id":"synthetic-account", "token":"synthetic-token"}), "synthetic-account", "synthetic-token", true).is_err());
    }
}

#[tauri::command(async)]
pub(crate) fn account_credential_write(app: AppHandle, credential: Value) -> Result<(), String> {
    crate::native_log::logged_without_detail("account_credential_write", (|| {
        let serialized =
            serde_json::to_vec(&credential).map_err(|_| "account credential is invalid".to_owned())?;
        slot(&app)?
            .write(&SecretBytes(zeroize::Zeroizing::new(serialized)))
            .map_err(|error| format!("account credential could not be stored: {error}"))
    })())
}

#[tauri::command(async)]
pub(crate) fn account_credential_clear(app: AppHandle) -> Result<(), String> {
    crate::native_log::logged_without_detail("account_credential_clear", (|| {
        slot(&app)?
            .remove()
            .map_err(|error| format!("account credential could not be removed: {error}"))
    })())
}
