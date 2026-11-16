//! Native external-storage connection, authorization and recovery commands.
//! Network operations never run while the library PDS mutex is held.
use super::{
    auth::{SecretBytes, SecretVault},
    connection::{self, *},
    connection_store::{ConnectionLabels, ConnectionStore, PendingStoredConnection, StoredConnection},
    contract::{
        Cancellation, ErrorKind, Provider, ProviderError, RepositoryHandle, Result, SecretRef,
    },
    descriptor,
    providers::{self, Dependencies},
    recovery, runtime, secrets,
};
use crate::native_log::logged;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use risunest_external_storage_format::{crypto::root_key, format::Descriptor};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::{Read, Seek, Write},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_fs::{FsExt, OpenOptions};
use zeroize::{Zeroize, Zeroizing};

pub(crate) struct ConnectedRepository {
    pub stored: StoredConnection,
    pub provider: Arc<dyn Provider>,
    pub handle: RepositoryHandle,
    pub dependencies: Dependencies,
    pub root_key: Zeroizing<[u8; 32]>,
}

struct PendingPreparation {
    request: PrepareConnectionRequest,
    expires_at_ms: u64,
    recovery_key: Option<Zeroizing<String>>,
    expected_repository_id: Option<String>,
    imported_credential: Option<ImportedCredential>,
    bound_credential: Option<BoundCredential>,
    selected_folder_name: Option<String>,
    /// Folder name carried by imported connection settings.
    imported_folder_name: Option<String>,
    account_label: Option<String>,
    transferred: bool,
    renewal: Option<String>,
    /// The account a renewal has to sign in with, so the sign-in page selects it.
    login_hint: Option<String>,
}

#[derive(Clone)]
struct BoundCredential {
    reference: SecretRef,
    account_id: String,
    account_label: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct FolderEntry {
    name: String,
    handle: String,
}

#[derive(Clone)]
struct FolderNode {
    id: String,
    path: Vec<FolderEntry>,
}

struct FolderCursor {
    folder_id: String,
    provider_cursor: String,
}

struct FolderSelectionSession {
    preparation_id: String,
    expires_at_ms: u64,
    account_hint: Option<String>,
    drive_id: String,
    root_item_id: String,
    handles: HashMap<String, FolderNode>,
    cursors: HashMap<String, FolderCursor>,
}

struct ImportedCredential {
    bytes: Zeroizing<Vec<u8>>,
    account_id: Option<String>,
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
struct PendingAuthorization {
    preparation_id: String,
    expires_at_ms: u64,
    flow: super::oauth::LoopbackAuthorization,
    exchange_config: super::contract::ConnectionConfig,
}

#[cfg(target_os = "ios")]
struct PendingAuthorization {
    preparation_id: String,
    expires_at_ms: u64,
    grant: super::auth::AuthorizationCode,
    exchange_config: super::contract::ConnectionConfig,
}

#[cfg(target_os = "android")]
enum PendingAuthorization {
    Google {
        preparation_id: String,
        expires_at_ms: u64,
        flow: super::oauth::AndroidRedirectAuthorization,
        exchange_config: super::contract::ConnectionConfig,
    },
    OneDrive {
        preparation_id: String,
        expires_at_ms: u64,
        flow: super::oauth::AndroidRedirectAuthorization,
        exchange_config: super::contract::ConnectionConfig,
    },
}

#[derive(Clone)]
struct PendingConnectionSettings {
    connection_id: String,
    expires_at_ms: u64,
    payload: String,
}

#[derive(Default)]
pub(crate) struct ConnectionCommandState {
    activity: Arc<Mutex<ConnectionActivity>>,
    preparations: Mutex<HashMap<String, PendingPreparation>>,
    authorizations: Mutex<HashMap<String, PendingAuthorization>>,
    authorization_cancellations: Mutex<HashMap<String, Cancellation>>,
    folder_selections: Mutex<HashMap<String, FolderSelectionSession>>,
    connection_settings: Mutex<HashMap<String, PendingConnectionSettings>>,
}

#[derive(Default)]
struct ConnectionActivity {
    closed: bool,
    active: usize,
}

struct ConnectionCommandGuard(Arc<Mutex<ConnectionActivity>>);
impl Drop for ConnectionCommandGuard {
    fn drop(&mut self) {
        if let Ok(mut activity) = self.0.lock() {
            activity.active -= 1;
        }
    }
}

impl ConnectionCommandState {
    fn connection_settings_for_save(&self, transfer_id: &str, at_ms: u64) -> Result<PendingConnectionSettings> {
        let mut settings = lock(&self.connection_settings)?;
        let pending = settings.get(transfer_id)
            .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        if pending.expires_at_ms <= at_ms {
            settings.remove(transfer_id);
            return Err(ProviderError::new(ErrorKind::Cancelled));
        }
        Ok(pending.clone())
    }

    fn admit(&self) -> Result<ConnectionCommandGuard> {
        let mut activity = lock(&self.activity)?;
        if activity.closed {
            return Err(ProviderError::new(ErrorKind::Cancelled));
        }
        activity.active += 1;
        Ok(ConnectionCommandGuard(self.activity.clone()))
    }

    pub(crate) fn begin_cleanup(&self) -> Result<()> {
        lock(&self.activity)?.closed = true;
        for cancel in lock(&self.authorization_cancellations)?.values() {
            cancel.cancel();
        }
        Ok(())
    }

    pub(crate) fn cleanup_drained(&self) -> Result<bool> {
        if lock(&self.activity)?.active != 0 {
            return Ok(false);
        }
        lock(&self.preparations)?.clear();
        lock(&self.authorizations)?.clear();
        lock(&self.authorization_cancellations)?.clear();
        lock(&self.folder_selections)?.clear();
        lock(&self.connection_settings)?.clear();
        Ok(true)
    }

    pub(crate) fn finish_cleanup(&self) -> Result<()> {
        let mut activity = lock(&self.activity)?;
        if activity.active != 0 {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        activity.closed = false;
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CommitConnectionRequest {
    preparation_id: String,
    secret: Option<ProviderSecretInput>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct BeginAuthorizationRequest {
    preparation_id: String,
    current_platform_client_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CompleteAuthorizationRequest {
    authorization_id: String,
    redirect_url: Option<String>,
    client_secret: Option<String>,
}

fn authorization_client_secret(
    provider: &str,
    client_secret: Option<String>,
) -> Result<Option<Zeroizing<String>>> {
    let client_secret = client_secret.map(Zeroizing::new);
    if client_secret.is_some() && provider != "google_drive" {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    Ok(client_secret)
}

#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum CompleteAuthorizationResult {
    Connected(ConnectionResult),
    Pending {
        #[serde(rename = "authorizationPending")]
        authorization_pending: bool,
        #[serde(
            rename = "callbackRejected",
            skip_serializing_if = "std::ops::Not::not"
        )]
        callback_rejected: bool,
    },
    FolderSelected {
        #[serde(rename = "folderSelected")]
        folder_selected: bool,
        folder: FolderSelection,
    },
    FolderSelectionRequired {
        #[serde(rename = "folderSelectionRequired")]
        folder_selection_required: bool,
        #[serde(rename = "selectionId")]
        selection_id: String,
        #[serde(rename = "accountHint", skip_serializing_if = "Option::is_none")]
        account_hint: Option<String>,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FolderSelection {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_hint: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FolderPage {
    path: Vec<FolderEntry>,
    folders: Vec<FolderEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
    selectable: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ListFoldersRequest {
    selection_id: String,
    folder: Option<String>,
    cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SelectFolderRequest {
    selection_id: String,
    folder: String,
}

/// A connect command's failure. A repository this device already holds is
/// refused with its own kind so the form can name it; everything else is the
/// provider failure as it happened.
#[derive(Serialize)]
#[serde(untagged)]
pub(crate) enum ConnectionFailure {
    Refused { kind: &'static str },
    Provider(ProviderError),
}

impl ConnectionFailure {
    const ALREADY_CONNECTED: Self = Self::Refused {
        kind: "alreadyConnected",
    };
}

impl From<ProviderError> for ConnectionFailure {
    fn from(value: ProviderError) -> Self {
        Self::Provider(value)
    }
}

impl crate::native_log::CommandFailure for ConnectionFailure {
    fn code(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Self::Refused { kind } => (*kind).into(),
            Self::Provider(error) => error.code(),
        }
    }
    fn detail(&self) -> Option<std::borrow::Cow<'_, str>> {
        match self {
            Self::Refused { .. } => None,
            Self::Provider(error) => error.detail(),
        }
    }
    fn expected(&self) -> bool {
        match self {
            Self::Refused { .. } => true,
            Self::Provider(error) => error.expected(),
        }
    }
}

type ConnectResult<T> = std::result::Result<T, ConnectionFailure>;

#[tauri::command]
pub(crate) fn external_storage_cancel_authorization(
    state: State<'_, ConnectionCommandState>,
    authorization_id: String,
) -> Result<()> {
    logged("external_storage_cancel_authorization", (|| {
        let _cleanup_guard = state.admit()?;
        cancel_authorization(&state, &authorization_id)
    })())
}

fn cancel_authorization(state: &ConnectionCommandState, authorization_id: &str) -> Result<()> {
    lock(&state.authorizations)?.remove(authorization_id);
    if let Some(cancel) = lock(&state.authorization_cancellations)?.remove(authorization_id) {
        cancel.cancel();
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrepareConnectionSettingsImportRequest {
    payload: String,
    recovery_key: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PendingAuthorizationSummary {
    authorization_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    authorization_url: Option<String>,
    expires_at_ms: String,
    state: &'static str,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecoveryKeyMaterial {
    key: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectionSettingsMaterial {
    transfer_id: String,
    expires_at_ms: String,
    payload: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectionResult {
    connection: ConnectionSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<RecoveryKeyMaterial>,
}

fn now_ms() -> u64 {
    runtime::now_ms()
}

fn lock<T>(value: &Mutex<T>) -> Result<std::sync::MutexGuard<'_, T>> {
    value
        .lock()
        .map_err(|_| ProviderError::new(ErrorKind::Transient))
}

fn take_preparation(state: &ConnectionCommandState, id: &str) -> Result<PendingPreparation> {
    let pending = lock(&state.preparations)?
        .remove(id)
        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
    if pending.expires_at_ms <= now_ms() {
        return Err(ProviderError::new(ErrorKind::Cancelled));
    }
    Ok(pending)
}

fn restore_preparation(state: &ConnectionCommandState, id: String, pending: PendingPreparation) {
    if pending.expires_at_ms > now_ms() {
        if let Ok(mut preparations) = state.preparations.lock() {
            preparations.insert(id, pending);
        }
    }
}

fn insert_preparation(
    state: &ConnectionCommandState,
    mut request: PrepareConnectionRequest,
    expected_repository_id: Option<String>,
    imported_credential: Option<ImportedCredential>,
    imported_folder_name: Option<String>,
    transferred: bool,
) -> Result<PreparedConnection> {
    if transferred && request.config.oauth_profile.is_some() && request.config.account_id.is_empty()
    {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let endpoint = connection::validate_preparation(&request)?;
    let recovery_key = request
        .recovery_key
        .take()
        .map(|value| {
            risunest_external_storage_format::crypto::RecoveryKey::parse(&value)
                .map_err(|_| ProviderError::new(ErrorKind::RecoveryKeyMismatch))?;
            Ok(Zeroizing::new(value))
        })
        .transpose()?;
    let preparation_id = uuid::Uuid::new_v4().to_string();
    let expires_at_ms = now_ms().saturating_add(PREPARATION_LIFETIME_MS);
    let missing_platform_client = transferred
        && request
            .config
            .oauth_profile
            .as_ref()
            .is_some_and(|profile| !profile.platform_client_ids.contains_key(platform_key()));
    let result = PreparedConnection {
        preparation_id: preparation_id.clone(),
        expires_at_ms: expires_at_ms.to_string(),
        endpoint,
        capabilities: None,
        requires_o_auth: request.config.oauth_profile.is_some(),
        requires_recovery_key: false,
        requires_platform_o_auth_client: missing_platform_client,
        requires_folder_selection: connection::requires_folder_selection(&request),
        oauth_project_hint: missing_platform_client.then(|| {
            request
                .config
                .oauth_profile
                .as_ref()
                .expect("checked OAuth profile")
                .project_id
                .clone()
        }),
    };
    lock(&state.preparations)?.insert(
        preparation_id,
        PendingPreparation {
            request,
            expires_at_ms,
            recovery_key,
            expected_repository_id,
            imported_credential,
            bound_credential: None,
            selected_folder_name: None,
            imported_folder_name,
            account_label: None,
            transferred,
            renewal: None,
            login_hint: None,
        },
    );
    Ok(result)
}

fn apply_recovery_platform_client(
    preparation: &mut PendingPreparation,
    supplied: Option<String>,
) -> Result<()> {
    let Some(profile) = preparation.request.config.oauth_profile.as_mut() else {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    };
    if let Some(current) = profile.platform_client_ids.get(platform_key()) {
        return if supplied.is_none()
            || (preparation.transferred && supplied.as_deref() == Some(current.as_str()))
        {
            Ok(())
        } else {
            Err(ProviderError::new(ErrorKind::Unsupported))
        };
    }
    if !preparation.transferred {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let supplied = supplied.ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
    if supplied.is_empty()
        || supplied.len() > 1024
        || supplied.chars().any(char::is_control)
        || supplied.trim() != supplied
    {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let same_app = match preparation.request.config.provider.as_str() {
        "google_drive" => {
            let project = profile
                .platform_client_ids
                .values()
                .map(|client| google_project_number(client))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
            let supplied_project = google_project_number(&supplied)
                .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
            !project.is_empty() && project.iter().all(|project| *project == supplied_project)
        }
        "onedrive" => {
            let known = uuid::Uuid::parse_str(&profile.project_id).ok();
            let supplied = uuid::Uuid::parse_str(&supplied).ok();
            known.is_some() && known == supplied
        }
        _ => false,
    };
    if !same_app {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    profile
        .platform_client_ids
        .insert(platform_key().into(), supplied);
    Ok(())
}

fn authorization_config(
    state: &ConnectionCommandState,
    preparation_id: &str,
    current_platform_client_id: Option<String>,
) -> Result<super::contract::ConnectionConfig> {
    let mut pending = lock(&state.preparations)?;
    let preparation = pending
        .get_mut(preparation_id)
        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
    if preparation.expires_at_ms <= now_ms() {
        return Err(ProviderError::new(ErrorKind::Cancelled));
    }
    if preparation.request.mode == ConnectionOpenMode::Existing
        && preparation.recovery_key.is_none()
        && preparation.renewal.is_none()
    {
        return Err(ProviderError::new(ErrorKind::ReauthRequired));
    }
    apply_recovery_platform_client(preparation, current_platform_client_id)?;
    let mut config = preparation.request.config.clone();
    if config.provider == "onedrive" {
        let app_folder =
            config.location.get("accountType").map(String::as_str) == Some("appFolder");
        config
            .location
            .entry("driveId".into())
            .or_insert_with(|| "pending".into());
        config
            .location
            .entry("rootItemId".into())
            .or_insert_with(|| {
                if app_folder {
                    "special/approot".into()
                } else {
                    "pending".into()
                }
            });
        config.location.remove("folderName");
    }
    Ok(config)
}

fn validate_authenticated_account(
    preparation: &PendingPreparation,
    account_id: Option<&String>,
) -> Result<()> {
    if account_id.is_some_and(|account_id| {
        preparation.request.mode == ConnectionOpenMode::Existing
            && !preparation.request.config.account_id.is_empty()
            && account_id != &preparation.request.config.account_id
    }) {
        return Err(ProviderError::new(ErrorKind::ReauthRequired));
    }
    Ok(())
}

async fn exchange_oauth_credential(
    root: &std::path::Path,
    dependencies: &Dependencies,
    exchange_config: &super::contract::ConnectionConfig,
    grant: &super::auth::AuthorizationCode,
    client_secret: Option<Zeroizing<String>>,
    renewal: Option<&str>,
    cancel: &Cancellation,
) -> Result<BoundCredential> {
    match exchange_config.provider.as_str() {
        "google_drive" => {
            let replaced = renewal
                .and_then(|connection_id| ConnectionStore::open(root).and_then(|store| store.read(connection_id)).ok())
                .map(|stored| SecretRef(stored.credential_ref));
            let authorized = providers::google_drive::auth::exchange_authorization_code(
                dependencies,
                exchange_config,
                grant,
                client_secret,
                replaced.as_ref(),
                cancel,
            )
            .await?;
            Ok(BoundCredential {
                reference: dependencies.vault.store(&authorized.secret).await?,
                account_id: authorized.account_id,
                account_label: authorized.account_label,
            })
        }
        "onedrive" => {
            if client_secret.is_some() {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            let provider = providers::onedrive::OneDrive::new(dependencies.clone());
            let authorized = provider
                .exchange_authorization_code(exchange_config, grant, cancel)
                .await?;
            Ok(BoundCredential {
                reference: authorized.secret,
                account_id: authorized.account_id,
                account_label: authorized.account_label,
            })
        }
        _ => Err(ProviderError::new(ErrorKind::Unsupported)),
    }
}

async fn finish_oauth_connection(
    app: &AppHandle,
    state: &ConnectionCommandState,
    preparation_id: String,
    mut pending: PendingPreparation,
    exchange_config: super::contract::ConnectionConfig,
    grant: super::auth::AuthorizationCode,
    client_secret: Option<Zeroizing<String>>,
    cancel: &Cancellation,
) -> ConnectResult<CompleteAuthorizationResult> {
    let root = connection_root(app)?;
    let dependencies = connection::dependencies(&root)?;
    let credential = match exchange_oauth_credential(
        &root,
        &dependencies,
        &exchange_config,
        &grant,
        client_secret,
        pending.renewal.as_deref(),
        cancel,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            restore_preparation(state, preparation_id, pending);
            return Err(error.into());
        }
    };
    if let Err(error) = validate_authenticated_account(&pending, Some(&credential.account_id)) {
        let _ = dependencies.vault.remove(&credential.reference).await;
        restore_preparation(state, preparation_id, pending);
        return Err(error.into());
    }
    pending.request.config.account_id = credential.account_id.clone();
    pending.account_label = credential.account_label.clone();

    if pending.renewal.is_some() {
        let committed = commit_preparation(app, &preparation_id, &pending,
            CredentialInput::Reference { reference: credential.reference, account_id: Some(credential.account_id) }, cancel).await;
        if committed.is_err() { restore_preparation(state, preparation_id, pending); }
        return committed.map(CompleteAuthorizationResult::Connected);
    }

    let setup_result: Result<Option<CompleteAuthorizationResult>> = async {
        match pending.request.config.provider.as_str() {
            "google_drive" => {
                let space = pending
                    .request
                    .config
                    .location
                    .get("space")
                    .map(String::as_str)
                    .unwrap_or("drive");
                if space == "appDataFolder" {
                    pending
                        .request
                        .config
                        .location
                        .insert("folderId".into(), "appDataFolder".into());
                    Ok(None)
                } else if let Some(name) =
                    pending.request.config.location.get("folderName").cloned()
                {
                    let provider = providers::google_drive::GoogleDrive::new(dependencies.clone());
                    let folder = provider
                        .create_setup_folder(
                            &pending.request.config,
                            &credential.reference,
                            &credential.account_id,
                            &name,
                            cancel,
                        )
                        .await?;
                    pending.request.config.location.remove("folderName");
                    pending
                        .request
                        .config
                        .location
                        .insert("folderId".into(), folder.id);
                    pending.selected_folder_name = Some(folder.name);
                    Ok(None)
                } else if pending.request.config.location.contains_key("folderId") {
                    Ok(None)
                } else {
                    let folder_id = grant
                        .picked_file_id
                        .as_deref()
                        .ok_or_else(|| ProviderError::new(ErrorKind::FolderInaccessible))?;
                    let provider = providers::google_drive::GoogleDrive::new(dependencies.clone());
                    let folder = provider
                        .inspect_setup_folder(
                            &pending.request.config,
                            &credential.reference,
                            &credential.account_id,
                            folder_id,
                            cancel,
                        )
                        .await?;
                    pending
                        .request
                        .config
                        .location
                        .insert("folderId".into(), folder.id);
                    pending.selected_folder_name = Some(folder.name.clone());
                    Ok(Some(CompleteAuthorizationResult::FolderSelected {
                        folder_selected: true,
                        folder: FolderSelection {
                            name: folder.name,
                            account_hint: credential.account_label.clone(),
                        },
                    }))
                }
            }
            "onedrive" => {
                let provider = providers::onedrive::OneDrive::new(dependencies.clone());
                let account_type = pending
                    .request
                    .config
                    .location
                    .get("accountType")
                    .map(String::as_str);
                if pending.request.config.location.contains_key("driveId")
                    && pending.request.config.location.contains_key("rootItemId")
                {
                    Ok(None)
                } else {
                    let drive = provider
                        .resolve_setup_drive(
                            &pending.request.config,
                            &credential.reference,
                            &credential.account_id,
                            cancel,
                        )
                        .await?;
                    if account_type == Some("appFolder") {
                        pending
                            .request
                            .config
                            .location
                            .insert("driveId".into(), drive.drive_id);
                        pending
                            .request
                            .config
                            .location
                            .insert("rootItemId".into(), drive.root_item_id);
                        Ok(None)
                    } else if let Some(name) =
                        pending.request.config.location.get("folderName").cloned()
                    {
                        let folder = provider
                            .create_setup_folder(
                                &pending.request.config,
                                &credential.reference,
                                &credential.account_id,
                                &drive.drive_id,
                                &drive.root_item_id,
                                &name,
                                cancel,
                            )
                            .await?;
                        if pending.request.purpose == ConnectionPurpose::Sync {
                            pending.request.config.location.insert("syncRootPath".into(), folder.sync_root_path()?.into());
                        }
                        pending.request.config.location.remove("folderName");
                        pending
                            .request
                            .config
                            .location
                            .insert("driveId".into(), drive.drive_id);
                        pending
                            .request
                            .config
                            .location
                            .insert("rootItemId".into(), folder.id);
                        pending.selected_folder_name = Some(folder.name);
                        Ok(None)
                    } else {
                        let selection_id = uuid::Uuid::new_v4().to_string();
                        lock(&state.folder_selections)?.insert(
                            selection_id.clone(),
                            FolderSelectionSession {
                                preparation_id: preparation_id.clone(),
                                expires_at_ms: pending.expires_at_ms,
                                account_hint: credential.account_label.clone(),
                                drive_id: drive.drive_id,
                                root_item_id: drive.root_item_id,
                                handles: HashMap::new(),
                                cursors: HashMap::new(),
                            },
                        );
                        Ok(Some(CompleteAuthorizationResult::FolderSelectionRequired {
                            folder_selection_required: true,
                            selection_id,
                            account_hint: credential.account_label.clone(),
                        }))
                    }
                }
            }
            _ => Err(ProviderError::new(ErrorKind::Unsupported)),
        }
    }
    .await;
    let outcome = match setup_result {
        Ok(value) => value,
        Err(error) => {
            let _ = dependencies.vault.remove(&credential.reference).await;
            restore_preparation(state, preparation_id, pending);
            return Err(error.into());
        }
    };
    if let Some(outcome) = outcome {
        if let Some(previous) = pending.bound_credential.replace(credential) {
            let _ = dependencies.vault.remove(&previous.reference).await;
        }
        restore_preparation(state, preparation_id, pending);
        return Ok(outcome);
    }
    let committed = commit_preparation(
        app,
        &preparation_id,
        &pending,
        CredentialInput::Reference {
            reference: credential.reference,
            account_id: Some(credential.account_id),
        },
        cancel,
    )
    .await;
    if committed.is_err() {
        restore_preparation(state, preparation_id, pending);
    }
    committed.map(CompleteAuthorizationResult::Connected)
}

fn google_project_number(client_id: &str) -> Option<&str> {
    let (project, suffix) = client_id.split_once('-')?;
    (!project.is_empty()
        && project.bytes().all(|byte| byte.is_ascii_digit())
        && !suffix.is_empty()
        && client_id.ends_with(".apps.googleusercontent.com"))
    .then_some(project)
}

pub(crate) fn connection_root<R: tauri::Runtime>(app: &AppHandle<R>) -> Result<PathBuf> {
    runtime::root(app)
}

pub(crate) fn budget(app: &AppHandle) -> Result<super::durable_quota::MyboxBudget> {
    Ok(super::durable_quota::MyboxBudget::new(
        connection_root(app)?.join("account-quota.sqlite"),
    ))
}

pub(crate) fn summary(connection: &StoredConnection) -> Result<ConnectionSummary> {
    connection
        .descriptor
        .validate()
        .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    connection::validate_config_shape(&connection.config)
        .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    if connection.config.oauth_profile.is_some() && connection.config.account_id.is_empty() {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    require_repository_strategy(
        &connection.capabilities,
        connection.descriptor.publication_strategy,
    )?;
    Ok(connection::summary(connection))
}

#[tauri::command]
pub(crate) fn external_storage_list_providers() -> Vec<ProviderDescriptor> {
    provider_descriptors()
}

#[tauri::command]
pub(crate) fn external_storage_prepare_connection(
    state: State<'_, ConnectionCommandState>,
    request: PrepareConnectionRequest,
) -> Result<PreparedConnection> {
    logged("external_storage_prepare_connection", (|| {
        let _cleanup_guard = state.admit()?;
        insert_preparation(&state, request, None, None, None, false)
    })())
}

#[tauri::command]
pub(crate) fn external_storage_prepare_renewal(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    connection_id: String,
) -> Result<PreparedConnection> {
    logged("external_storage_prepare_renewal", (|| {
        let _cleanup_guard = state.admit()?;
        let stored = ConnectionStore::open(&connection_root(&app)?)?.read(&connection_id)?;
        insert_renewal(&state, stored, connection_id)
    })())
}

fn insert_renewal(state: &ConnectionCommandState, stored: StoredConnection, connection_id: String) -> Result<PreparedConnection> {
    let preparation_id = uuid::Uuid::new_v4().to_string();
    let expires_at_ms = now_ms().saturating_add(PREPARATION_LIFETIME_MS);
    let prepared = PreparedConnection {
        preparation_id: preparation_id.clone(), expires_at_ms: expires_at_ms.to_string(),
        endpoint: connection::stored_endpoint(&stored)?, capabilities: Some(stored.capabilities.clone()),
        requires_o_auth: stored.config.oauth_profile.is_some(), requires_recovery_key: false,
        requires_platform_o_auth_client: false, requires_folder_selection: false, oauth_project_hint: None,
    };
    let purpose = if stored.descriptor.publication_strategy.is_none() { ConnectionPurpose::Backup } else { ConnectionPurpose::Sync };
    lock(&state.preparations)?.insert(preparation_id, PendingPreparation {
        request: PrepareConnectionRequest { config: stored.config, mode: ConnectionOpenMode::Existing,
            purpose, recovery_key: None, acknowledgements: vec![] },
        expires_at_ms, recovery_key: None, expected_repository_id: Some(stored.descriptor.repository_id),
        imported_credential: None, bound_credential: None, selected_folder_name: None,
        imported_folder_name: None, account_label: None, transferred: false,
        renewal: Some(connection_id), login_hint: stored.labels.account,
    });
    Ok(prepared)
}

/// The account the provider's sign-in page selects for a renewal; a new connection has none.
fn authorization_login_hint(state: &ConnectionCommandState, preparation_id: &str) -> Result<Option<String>> {
    Ok(lock(&state.preparations)?.get(preparation_id).and_then(|preparation| preparation.login_hint.clone()))
}

#[tauri::command]
pub(crate) async fn external_storage_change_endpoint(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    connection_id: String,
    endpoint: String,
) -> Result<ConnectionResult> {
    logged("external_storage_change_endpoint", async move {
        let _cleanup_guard = state.admit()?;
        let root = connection_root(&app)?;
        let stored = ConnectionStore::open(&root)?.read(&connection_id)?;
        let key = read_root_key(secrets::repository_key_vault(&root).as_ref(), &stored.root_key_ref).await?;
        change_endpoint(&root, &stored, endpoint, |config| connection::dependencies_for_config(&root, config), &key, &Cancellation::default()).await
    }.await)
}

pub(super) async fn change_endpoint(
    root: &std::path::Path,
    stored: &StoredConnection,
    endpoint: String,
    dependencies: impl FnOnce(&super::contract::ConnectionConfig) -> Result<Dependencies>,
    key: &[u8; 32],
    cancel: &Cancellation,
) -> Result<ConnectionResult> {
    if !matches!(stored.config.provider.as_str(), "webdav" | "s3") {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let mut config = stored.config.clone();
    config.endpoint = endpoint.trim().to_owned();
    let provider = connection::provider_for(&config, dependencies(&config)?)?;
    let (handle, capabilities) = provider.open_repository(&config, &SecretRef(stored.credential_ref.clone()),
        super::contract::OpenMode::Existing, cancel).await?;
    if handle.repository_id != stored.provider_repository_id
        || handle.connection_identity != stored.descriptor_locator.connection_identity {
        return Err(ProviderError::new(ErrorKind::RepositoryMismatch));
    }
    require_repository_strategy(&capabilities, stored.descriptor.publication_strategy)?;
    descriptor::read(root, provider.as_ref(), &handle, &stored.descriptor_locator, &stored.descriptor, key, cancel).await
        .map_err(|error| if error.kind == ErrorKind::NotFound { ProviderError::new(ErrorKind::RepositoryMismatch) } else { error })?;
    cancel.check()?;
    let connection = ConnectionStore::open(root)?.replace_endpoint(stored, config.endpoint)?;
    Ok(ConnectionResult { connection: connection::summary(&connection), recovery: None })
}

async fn renew_connection(
    app: &AppHandle,
    connection_id: &str,
    credential: CredentialInput,
    account_label: Option<String>,
    cancel: &Cancellation,
) -> Result<ConnectionResult> {
    let root = connection_root(app)?;
    let stored = ConnectionStore::open(&root)?.read(connection_id)?;
    let dependencies = connection::dependencies_for_config(&root, &stored.config)?;
    let (replacement, account_id) = match credential {
        CredentialInput::Bytes(secret) => (dependencies.vault.store(&secret.bytes).await?, secret.account_id),
        CredentialInput::Reference { reference, account_id } => (reference, account_id),
    };
    let result = async {
        require_renewal_account(&stored.config, account_id.as_deref())?;
        let provider = connection::provider_for(&stored.config, dependencies.clone())?;
        let (handle, capabilities) = provider.open_repository(&stored.config, &replacement,
            super::contract::OpenMode::Existing, cancel).await?;
        require_renewal_repository(&stored, &handle)?;
        require_repository_strategy(&capabilities, stored.descriptor.publication_strategy)?;
        let key = read_root_key(secrets::repository_key_vault(&root).as_ref(), &stored.root_key_ref).await?;
        descriptor::read(&root, provider.as_ref(), &handle, &stored.descriptor_locator,
            &stored.descriptor, &key, cancel).await?;
        cancel.check()?;
        ConnectionStore::open(&root)?.replace_credential(connection_id, &stored.credential_ref, &replacement.0, account_label)
    }.await;
    complete_credential_renewal(dependencies.vault.as_ref(), &SecretRef(stored.credential_ref), &replacement, result).await
}

async fn complete_credential_renewal(
    vault: &dyn SecretVault,
    previous: &SecretRef,
    replacement: &SecretRef,
    result: Result<StoredConnection>,
) -> Result<ConnectionResult> {
    match result {
        Ok(connection) => {
            let _ = vault.remove(previous).await;
            Ok(ConnectionResult { connection: connection::summary(&connection), recovery: None })
        }
        Err(error) => { let _ = vault.remove(replacement).await; Err(error) }
    }
}

fn require_renewal_account(config: &super::contract::ConnectionConfig, account: Option<&str>) -> Result<()> {
    if config.oauth_profile.is_some() && account != Some(config.account_id.as_str()) {
        return Err(ProviderError::new(ErrorKind::ReauthRequired));
    }
    Ok(())
}

fn require_renewal_repository(stored: &StoredConnection, handle: &RepositoryHandle) -> Result<()> {
    if handle.repository_id != stored.provider_repository_id
        || handle.connection_identity != stored.descriptor_locator.connection_identity {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    Ok(())
}

#[tauri::command]
pub(crate) async fn external_storage_unlock_connection(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    connection_id: String,
    recovery_key: String,
) -> Result<()> {
    logged("external_storage_unlock_connection", async move {
        let _cleanup_guard = state.admit()?;
        let recovery_key = Zeroizing::new(recovery_key);
        let root = connection_root(&app)?;
        let stored = ConnectionStore::open(&root)?.read(&connection_id)?;
        let dependencies = connection::dependencies_for_config(&root, &stored.config)?;
        let provider = connection::provider_for(&stored.config, dependencies)?;
        let cancel = Cancellation::default();
        let (handle, _) = provider.open_repository(&stored.config, &SecretRef(stored.credential_ref.clone()),
            super::contract::OpenMode::Existing, &cancel).await?;
        require_renewal_repository(&stored, &handle)?;
        let recovered = recovery::open_bootstrap(&root, provider.as_ref(), &handle, &recovery_key, &cancel).await?;
        if recovered.metadata.descriptor != stored.descriptor
            || recovered.metadata.descriptor_locator != stored.descriptor_locator {
            return Err(ProviderError::new(ErrorKind::RepositoryMismatch));
        }
        descriptor::read(&root, provider.as_ref(), &handle, &stored.descriptor_locator,
            &stored.descriptor, &recovered.key, &cancel).await?;
        let vault = secrets::repository_key_vault(&root);
        let replacement = vault.store(&SecretBytes(Zeroizing::new(recovered.key.to_vec()))).await?;
        let adopted = ConnectionStore::open(&root).and_then(|mut store|
            store.replace_repository_key(&connection_id, &stored.root_key_ref, &replacement.0));
        match adopted {
            Ok(_) => { let _ = vault.remove(&SecretRef(stored.root_key_ref)).await; Ok(()) }
            Err(error) => { let _ = vault.remove(&replacement).await; Err(error) }
        }
    }.await)
}

#[tauri::command]
pub(crate) async fn external_storage_commit_connection(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: CommitConnectionRequest,
) -> ConnectResult<ConnectionResult> {
    logged("external_storage_commit_connection", Box::pin(async move {
        let _cleanup_guard = state.admit()?;
        let preparation_id = request.preparation_id;
        let mut pending = take_preparation(&state, &preparation_id)?;
        let credential = if pending.request.config.oauth_profile.is_some() {
            if request.secret.is_some() {
                restore_preparation(&state, preparation_id, pending);
                return Err(ProviderError::new(ErrorKind::Unsupported).into());
            }
            let Some(bound) = pending.bound_credential.take() else {
                restore_preparation(&state, preparation_id, pending);
                return Err(ProviderError::new(ErrorKind::Unsupported).into());
            };
            CredentialInput::Reference {
                reference: bound.reference,
                account_id: Some(bound.account_id),
            }
        } else {
            let secret = match (&pending.imported_credential, request.secret) {
                (Some(imported), None) => EncodedProviderSecret {
                    bytes: SecretBytes(Zeroizing::new(imported.bytes.to_vec())),
                    account_id: imported.account_id.clone(),
                },
                (None, Some(secret)) => {
                    connection::encode_secret(&pending.request.config.provider, secret)?
                }
                _ => {
                    restore_preparation(&state, preparation_id, pending);
                    return Err(ProviderError::new(ErrorKind::Unsupported).into());
                }
            };
            CredentialInput::Bytes(secret)
        };
        let cancel = Cancellation::default();
        match commit_preparation(&app, &preparation_id, &pending, credential, &cancel).await {
            Ok(result) => Ok(result),
            Err(error) => {
                let error = match error {
                    ConnectionFailure::Provider(provider)
                        if pending.selected_folder_name.is_some()
                            && pending.request.mode == ConnectionOpenMode::Existing
                            && provider.kind == ErrorKind::NotFound =>
                    {
                        ConnectionFailure::Provider(ProviderError::new(ErrorKind::FolderNotRepository))
                    }
                    other => other,
                };
                restore_preparation(&state, preparation_id, pending);
                Err(error)
            }
        }
    }).await)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub(crate) async fn external_storage_begin_authorization(
    state: State<'_, ConnectionCommandState>,
    request: BeginAuthorizationRequest,
) -> Result<PendingAuthorizationSummary> {
    logged("external_storage_begin_authorization", Box::pin(async move {
        let _cleanup_guard = state.admit()?;
        let BeginAuthorizationRequest {
            preparation_id,
            current_platform_client_id,
        } = request;
        let mut exchange_config =
            { authorization_config(&state, &preparation_id, current_platform_client_id)? };
        let login_hint = authorization_login_hint(&state, &preparation_id)?;
        let provider = exchange_config.provider.clone();
        let (flow, authorization_url) = match provider.as_str() {
            "google_drive" => {
                super::oauth::LoopbackAuthorization::start(|redirect| {
                    providers::google_drive::auth::native_authorization_policy(
                        &exchange_config,
                        redirect,
                    )
                    .map(|policy| policy.with_login_hint(login_hint))
                })
                .await?
            }
            "onedrive" => {
                super::oauth::LoopbackAuthorization::start(|redirect| {
                    exchange_config
                        .location
                        .insert("redirectUri".into(), redirect.to_string());
                    providers::onedrive::authorization_policy(&exchange_config, platform_key())
                        .map(|policy| policy.with_login_hint(login_hint))
                })
                .await?
            }
            _ => return Err(ProviderError::new(ErrorKind::Unsupported)),
        };
        let authorization_id = uuid::Uuid::new_v4().to_string();
        let expires_at_ms = now_ms().saturating_add(PREPARATION_LIFETIME_MS);
        lock(&state.authorizations)?.insert(
            authorization_id.clone(),
            PendingAuthorization {
                preparation_id,
                expires_at_ms,
                flow,
                exchange_config,
            },
        );
        lock(&state.authorization_cancellations)?
            .insert(authorization_id.clone(), Cancellation::default());
        Ok(PendingAuthorizationSummary {
            authorization_id,
            authorization_url: Some(authorization_url.to_string()),
            expires_at_ms: expires_at_ms.to_string(),
            state: "browser-required",
        })
    }).await)
}

#[cfg(target_os = "ios")]
#[tauri::command]
pub(crate) async fn external_storage_begin_authorization(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: BeginAuthorizationRequest,
) -> Result<PendingAuthorizationSummary> {
    logged("external_storage_begin_authorization", Box::pin(async move {
        let _cleanup_guard = state.admit()?;
        let BeginAuthorizationRequest {
            preparation_id,
            current_platform_client_id,
        } = request;
        let config = { authorization_config(&state, &preparation_id, current_platform_client_id)? };
        let login_hint = authorization_login_hint(&state, &preparation_id)?;
        let mut exchange_config = config;
        let flow = match exchange_config.provider.as_str() {
            "google_drive" => {
                let (policy, callback_scheme) =
                    providers::google_drive::auth::ios_authorization_policy(&exchange_config)?;
                let policy = policy.with_login_hint(login_hint);
                super::oauth::IosWebAuthenticationAuthorization::start(policy, callback_scheme, true)?
            }
            "onedrive" => {
                exchange_config.location.insert(
                    "redirectUri".into(),
                    providers::onedrive::IOS_REDIRECT_URI.into(),
                );
                let policy = providers::onedrive::authorization_policy(&exchange_config, "ios")?
                    .with_login_hint(login_hint);
                let callback_scheme = policy.redirect_url.scheme().to_owned();
                super::oauth::IosWebAuthenticationAuthorization::start(policy, callback_scheme, false)?
            }
            _ => return Err(ProviderError::new(ErrorKind::Unsupported)),
        };
        let grant = flow.authenticate(&app).await?;
        let authorization_id = uuid::Uuid::new_v4().to_string();
        let expires_at_ms = now_ms().saturating_add(PREPARATION_LIFETIME_MS);
        lock(&state.authorizations)?.insert(
            authorization_id.clone(),
            PendingAuthorization {
                preparation_id,
                expires_at_ms,
                grant,
                exchange_config,
            },
        );
        lock(&state.authorization_cancellations)?
            .insert(authorization_id.clone(), Cancellation::default());
        Ok(PendingAuthorizationSummary {
            authorization_id,
            authorization_url: None,
            expires_at_ms: expires_at_ms.to_string(),
            state: "complete",
        })
    }).await)
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) async fn external_storage_begin_authorization(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: BeginAuthorizationRequest,
) -> Result<PendingAuthorizationSummary> {
    logged("external_storage_begin_authorization", Box::pin(async move {
        let _cleanup_guard = state.admit()?;
        let BeginAuthorizationRequest {
            preparation_id,
            current_platform_client_id,
        } = request;
        let config = { authorization_config(&state, &preparation_id, current_platform_client_id)? };
        let login_hint = authorization_login_hint(&state, &preparation_id)?;
        let authorization_id = uuid::Uuid::new_v4().to_string();
        let expires_at_ms = now_ms().saturating_add(PREPARATION_LIFETIME_MS);
        let on_ready: Box<dyn FnOnce() + Send> = Box::new({
            let authorization_id = authorization_id.clone();
            move || {
                let _ = tauri::Emitter::emit(&app, "external-storage-authorization-ready", authorization_id);
            }
        });
        let (authorization, authorization_url, authorization_state) = match config.provider.as_str() {
            "google_drive" => {
                let policy = providers::google_drive::auth::android_web_authorization_policy(&config)?
                    .with_login_hint(login_hint);
                let (flow, url) = super::oauth::android_google_web_authorization(policy, on_ready)?;
                (
                    PendingAuthorization::Google {
                        preparation_id,
                        expires_at_ms,
                        flow,
                        exchange_config: config,
                    },
                    Some(url.to_string()),
                    "browser-required",
                )
            }
            "onedrive" => {
                let mut exchange_config = config;
                exchange_config.location.insert(
                    "redirectUri".into(),
                    super::oauth::ANDROID_ONEDRIVE_REDIRECT_URI.into(),
                );
                let policy = providers::onedrive::authorization_policy(&exchange_config, "android")?
                    .with_login_hint(login_hint);
                let (flow, url) = super::oauth::android_redirect_authorization(policy, on_ready)?;
                (
                    PendingAuthorization::OneDrive {
                        preparation_id,
                        expires_at_ms,
                        flow,
                        exchange_config,
                    },
                    Some(url.to_string()),
                    "browser-required",
                )
            }
            _ => return Err(ProviderError::new(ErrorKind::Unsupported)),
        };
        lock(&state.authorizations)?.insert(authorization_id.clone(), authorization);
        lock(&state.authorization_cancellations)?
            .insert(authorization_id.clone(), Cancellation::default());
        Ok(PendingAuthorizationSummary {
            authorization_id,
            authorization_url,
            expires_at_ms: expires_at_ms.to_string(),
            state: authorization_state,
        })
    }).await)
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub(crate) async fn external_storage_complete_authorization(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    mut request: CompleteAuthorizationRequest,
) -> ConnectResult<CompleteAuthorizationResult> {
    logged("external_storage_complete_authorization", Box::pin(async move {
        let _cleanup_guard = state.admit()?;
        if let Some(mut redirect_url) = request.redirect_url {
            redirect_url.zeroize();
            return Err(ProviderError::new(ErrorKind::Unsupported).into());
        }
        let client_secret = {
            let authorizations = lock(&state.authorizations)?;
            let authorization = authorizations
                .get(&request.authorization_id)
                .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
            authorization_client_secret(
                &authorization.exchange_config.provider,
                request.client_secret.take(),
            )?
        };
        let authorization = lock(&state.authorizations)?
            .remove(&request.authorization_id)
            .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        let cancel = lock(&state.authorization_cancellations)?
            .get(&request.authorization_id)
            .cloned()
            .ok_or_else(|| ProviderError::new(ErrorKind::Cancelled))?;
        if authorization.expires_at_ms <= now_ms() {
            lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
            return Err(ProviderError::new(ErrorKind::Cancelled).into());
        }
        let pending = match take_preparation(&state, &authorization.preparation_id) {
            Ok(pending) => pending,
            Err(error) => {
                lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
                return Err(error.into());
            }
        };
        let grant = match authorization.flow.wait(&cancel).await {
            Ok(value) => value,
            Err(error) => {
                lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
                restore_preparation(&state, authorization.preparation_id, pending);
                return Err(error.into());
            }
        };
        let result = finish_oauth_connection(
            &app,
            &state,
            authorization.preparation_id,
            pending,
            authorization.exchange_config,
            grant,
            client_secret,
            &cancel,
        )
        .await;
        lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
        result
    }).await)
}

#[cfg(target_os = "ios")]
#[tauri::command]
pub(crate) async fn external_storage_complete_authorization(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    mut request: CompleteAuthorizationRequest,
) -> ConnectResult<CompleteAuthorizationResult> {
    logged("external_storage_complete_authorization", Box::pin(async move {
        let _cleanup_guard = state.admit()?;
        if request.redirect_url.is_some() {
            return Err(ProviderError::new(ErrorKind::Unsupported).into());
        }
        let client_secret = {
            let authorizations = lock(&state.authorizations)?;
            let authorization = authorizations
                .get(&request.authorization_id)
                .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
            authorization_client_secret(
                &authorization.exchange_config.provider,
                request.client_secret.take(),
            )?
        };
        let authorization = lock(&state.authorizations)?
            .remove(&request.authorization_id)
            .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        let cancel = lock(&state.authorization_cancellations)?
            .get(&request.authorization_id)
            .cloned()
            .ok_or_else(|| ProviderError::new(ErrorKind::Cancelled))?;
        if authorization.expires_at_ms <= now_ms() {
            lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
            return Err(ProviderError::new(ErrorKind::Cancelled).into());
        }
        let pending = match take_preparation(&state, &authorization.preparation_id) {
            Ok(pending) => pending,
            Err(error) => {
                lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
                return Err(error.into());
            }
        };
        let result = finish_oauth_connection(
            &app,
            &state,
            authorization.preparation_id,
            pending,
            authorization.exchange_config,
            authorization.grant,
            client_secret,
            &cancel,
        )
        .await;
        lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
        result
    }).await)
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) async fn external_storage_complete_authorization(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: CompleteAuthorizationRequest,
) -> ConnectResult<CompleteAuthorizationResult> {
    logged("external_storage_complete_authorization", Box::pin(async move {
        let _cleanup_guard = state.admit()?;
        let redirect_url = request.redirect_url.map(Zeroizing::new);
        let client_secret = request.client_secret.map(Zeroizing::new);
        let cancel = lock(&state.authorization_cancellations)?
            .get(&request.authorization_id)
            .cloned()
            .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        let (authorization, grant) = {
            let mut authorizations = lock(&state.authorizations)?;
            let authorization = authorizations
                .get_mut(&request.authorization_id)
                .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
            let (expires_at_ms, flow) = match authorization {
                PendingAuthorization::Google {
                    expires_at_ms,
                    flow,
                    ..
                } => (*expires_at_ms, flow),
                PendingAuthorization::OneDrive {
                    expires_at_ms,
                    flow,
                    ..
                } => {
                    if redirect_url.is_some() || client_secret.is_some() {
                        return Err(ProviderError::new(ErrorKind::Unsupported).into());
                    }
                    (*expires_at_ms, flow)
                }
            };
            if expires_at_ms <= now_ms() {
                authorizations.remove(&request.authorization_id);
                lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
                return Err(ProviderError::new(ErrorKind::Cancelled).into());
            }
            let grant = match flow.try_complete(redirect_url.as_deref().map(String::as_str)) {
                Ok(Some(grant)) => grant,
                Ok(None) => {
                    return Ok(CompleteAuthorizationResult::Pending {
                        authorization_pending: true,
                        callback_rejected: false,
                    })
                }
                Err(_) if !flow.is_consumed() => {
                    return Ok(CompleteAuthorizationResult::Pending {
                        authorization_pending: true,
                        callback_rejected: true,
                    })
                }
                Err(error) => {
                    authorizations.remove(&request.authorization_id);
                    lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
                    return Err(error.into());
                }
            };
            let authorization = authorizations
                .remove(&request.authorization_id)
                .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
            (authorization, grant)
        };
        let preparation_id = match &authorization {
            PendingAuthorization::Google { preparation_id, .. }
            | PendingAuthorization::OneDrive { preparation_id, .. } => preparation_id.clone(),
        };
        let pending = match take_preparation(&state, &preparation_id) {
            Ok(pending) => pending,
            Err(error) => {
                lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
                return Err(error.into());
            }
        };
        let exchange_config = match authorization {
            PendingAuthorization::Google {
                exchange_config, ..
            }
            | PendingAuthorization::OneDrive {
                exchange_config, ..
            } => exchange_config,
        };
        let result = finish_oauth_connection(
            &app,
            &state,
            preparation_id,
            pending,
            exchange_config,
            grant,
            client_secret,
            &cancel,
        )
        .await;
        lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
        result
    }).await)
}

#[tauri::command]
pub(crate) async fn external_storage_list_folders(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: ListFoldersRequest,
) -> Result<FolderPage> {
    logged("external_storage_list_folders", Box::pin(async move {
        let _cleanup_guard = state.admit()?;
        let mut session = lock(&state.folder_selections)?
            .remove(&request.selection_id)
            .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        if session.expires_at_ms <= now_ms() {
            return Err(ProviderError::new(ErrorKind::Cancelled));
        }
        let (config, credential) = {
            let preparations = lock(&state.preparations)?;
            let pending = preparations
                .get(&session.preparation_id)
                .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
            let credential = pending
                .bound_credential
                .clone()
                .ok_or_else(|| ProviderError::new(ErrorKind::ReauthRequired))?;
            (pending.request.config.clone(), credential)
        };
        let current = request
            .folder
            .as_deref()
            .map(|handle| {
                session
                    .handles
                    .get(handle)
                    .cloned()
                    .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))
            })
            .transpose()?;
        let folder_id = current
            .as_ref()
            .map(|node| node.id.as_str())
            .unwrap_or(session.root_item_id.as_str());
        let provider_cursor = request
            .cursor
            .as_deref()
            .map(|cursor| {
                let cursor = session
                    .cursors
                    .get(cursor)
                    .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
                if cursor.folder_id != folder_id {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                Ok(cursor.provider_cursor.as_str())
            })
            .transpose()?;
        let root = connection_root(&app)?;
        let dependencies = connection::dependencies(&root)?;
        let provider = providers::onedrive::OneDrive::new(dependencies);
        let cancel = Cancellation::default();
        let page = provider
            .list_setup_folders(
                &config,
                &credential.reference,
                &credential.account_id,
                &session.drive_id,
                folder_id,
                provider_cursor,
                &cancel,
            )
            .await;
        let page = match page {
            Ok(page) => page,
            Err(error) => {
                lock(&state.folder_selections)?.insert(request.selection_id, session);
                return Err(error);
            }
        };
        let parent_path = current
            .as_ref()
            .map(|node| node.path.clone())
            .unwrap_or_default();
        let mut folders = Vec::with_capacity(page.folders.len());
        for folder in page.folders {
            let entry = FolderEntry {
                name: folder.name.clone(),
                handle: uuid::Uuid::new_v4().to_string(),
            };
            let mut path = parent_path.clone();
            path.push(entry.clone());
            session.handles.insert(
                entry.handle.clone(),
                FolderNode {
                    id: folder.id,
                    path,
                },
            );
            folders.push(entry);
        }
        let next_cursor = page.next_cursor.map(|provider_cursor| {
            let cursor = uuid::Uuid::new_v4().to_string();
            session.cursors.insert(
                cursor.clone(),
                FolderCursor {
                    folder_id: folder_id.to_owned(),
                    provider_cursor,
                },
            );
            cursor
        });
        let result = FolderPage {
            path: parent_path,
            folders,
            next_cursor,
            selectable: current.is_some(),
        };
        lock(&state.folder_selections)?.insert(request.selection_id, session);
        Ok(result)
    }).await)
}

#[tauri::command]
pub(crate) async fn external_storage_select_folder(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: SelectFolderRequest,
) -> Result<FolderSelection> {
    logged("external_storage_select_folder", Box::pin(async move {
        let _cleanup_guard = state.admit()?;
        let session = lock(&state.folder_selections)?
            .remove(&request.selection_id)
            .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        if session.expires_at_ms <= now_ms() {
            return Err(ProviderError::new(ErrorKind::Cancelled));
        }
        let node = session
            .handles
            .get(&request.folder)
            .cloned()
            .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        let mut pending = take_preparation(&state, &session.preparation_id)?;
        let credential = pending
            .bound_credential
            .clone()
            .ok_or_else(|| ProviderError::new(ErrorKind::ReauthRequired))?;
        let root = connection_root(&app)?;
        let dependencies = connection::dependencies(&root)?;
        let provider = providers::onedrive::OneDrive::new(dependencies);
        let cancel = Cancellation::default();
        let inspected = provider
            .inspect_setup_folder(
                &pending.request.config,
                &credential.reference,
                &credential.account_id,
                &session.drive_id,
                &node.id,
                &cancel,
            )
            .await;
        let folder = match inspected {
            Ok(folder) => folder,
            Err(error) => {
                restore_preparation(&state, session.preparation_id.clone(), pending);
                lock(&state.folder_selections)?.insert(request.selection_id, session);
                return Err(error);
            }
        };
        if pending.request.purpose == ConnectionPurpose::Sync {
            pending.request.config.location.insert("syncRootPath".into(), folder.sync_root_path()?.into());
        }
        pending
            .request
            .config
            .location
            .insert("driveId".into(), session.drive_id);
        pending
            .request
            .config
            .location
            .insert("rootItemId".into(), folder.id);
        pending.request.config.location.remove("folderName");
        pending.selected_folder_name = Some(folder.name.clone());
        restore_preparation(&state, session.preparation_id, pending);
        Ok(FolderSelection {
            name: folder.name,
            account_hint: session.account_hint,
        })
    }).await)
}

#[tauri::command]
pub(crate) fn external_storage_cancel_folder_selection(
    state: State<'_, ConnectionCommandState>,
    selection_id: String,
) -> Result<()> {
    logged("external_storage_cancel_folder_selection", (|| {
        let _cleanup_guard = state.admit()?;
        lock(&state.folder_selections)?.remove(&selection_id);
        Ok(())
    })())
}

enum CredentialInput {
    Bytes(EncodedProviderSecret),
    Reference {
        reference: SecretRef,
        account_id: Option<String>,
    },
}

/// Creates the descriptor of a new repository, at this app's format level
/// when the repository syncs, and returns the descriptor the repository holds.
#[allow(clippy::too_many_arguments)]
async fn ensure_create_descriptor(
    root: &std::path::Path,
    provider_id: &str,
    provider: &dyn Provider,
    handle: &RepositoryHandle,
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    resuming: bool,
    cancel: &Cancellation,
) -> Result<(super::contract::RemoteLocator, Descriptor)> {
    let resumed = if resuming {
        descriptor::resume_existing(root, provider, handle, descriptor, root_key, cancel).await?
    } else {
        None
    };
    let locator = match resumed {
        Some(locator) => locator,
        None if descriptor.publication_strategy.is_some() && super::lww_commands::syncs_through(provider_id) => {
            let leveled = descriptor.clone().with_format_level(crate::server_sync::library_format::LEVEL)
                .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
            descriptor::upload(root, provider, handle, &leveled, root_key, cancel).await?
        }
        None => descriptor::upload(root, provider, handle, descriptor, root_key, cancel).await?,
    };
    let created = descriptor::read(
        root, provider, handle, &locator, descriptor, root_key, cancel,
    )
    .await?;
    Ok((locator, created))
}

fn occupied_location_error(creating: bool, error: ProviderError) -> ProviderError {
    if creating && error.kind == ErrorKind::PreconditionFailed {
        ProviderError { kind: ErrorKind::LocationOccupied, ..error }
    } else { error }
}

async fn commit_preparation(
    app: &AppHandle,
    connection_id: &str,
    preparation: &PendingPreparation,
    credential: CredentialInput,
    cancel: &Cancellation,
) -> ConnectResult<ConnectionResult> {
    if let Some(connection_id) = preparation.renewal.as_deref() {
        return renew_connection(app, connection_id, credential, preparation.account_label.clone(), cancel)
            .await.map_err(Into::into);
    }
    commit_new_connection(&connection_root(app)?, connection_id, preparation, credential, cancel).await
}

async fn commit_new_connection(
    root: &std::path::Path,
    connection_id: &str,
    preparation: &PendingPreparation,
    credential: CredentialInput,
    cancel: &Cancellation,
) -> ConnectResult<ConnectionResult> {
    let root = root.to_path_buf();
    let dependencies = connection::dependencies_for_config(&root, &preparation.request.config)?;
    let provider_vault = dependencies.vault.clone();
    let key_vault = secrets::repository_key_vault(&root);
    let mut config = preparation.request.config.clone();
    let mut store = ConnectionStore::open(&root)?;
    let labels = ConnectionLabels::new(
        preparation.account_label.clone(),
        preparation.selected_folder_name.clone().or_else(|| preparation.imported_folder_name.clone()),
    );
    let provided_account_id = match &credential {
        CredentialInput::Bytes(secret) => secret.account_id.as_ref(),
        CredentialInput::Reference { account_id, .. } => account_id.as_ref(),
    };
    if let Err(error) = validate_authenticated_account(preparation, provided_account_id) {
        if let CredentialInput::Reference { reference, .. } = &credential {
            let _ = provider_vault.remove(reference).await;
        }
        return Err(error.into());
    }
    if let Some(account_id) = provided_account_id {
        config.account_id = account_id.clone();
    }
    validate_sync_folder_before_commit(
        &mut config, preparation.request.purpose, &credential, &dependencies, cancel,
    ).await?;
    let effective_connection_id = if preparation.request.mode == ConnectionOpenMode::Create
        && store
            .pending(connection_id)
            .is_err_and(|error| error.kind == ErrorKind::NotFound)
    {
        store
            .pending_create_for(&config)?
            .map_or_else(|| connection_id.to_owned(), |pending| pending.id)
    } else {
        connection_id.to_owned()
    };
    let connection_id = effective_connection_id.as_str();
    let (pending, resuming) = match store.pending(connection_id) {
        Ok(mut pending) => {
            if pending.create != (preparation.request.mode == ConnectionOpenMode::Create) {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed).into());
            }
            let (replacement, account_id) = match credential {
                CredentialInput::Bytes(secret) => (
                    provider_vault.store(&secret.bytes).await?,
                    secret.account_id,
                ),
                CredentialInput::Reference {
                    reference,
                    account_id,
                } => (reference, account_id),
            };
            if account_id
                .as_ref()
                .is_some_and(|account_id| account_id != &pending.config.account_id)
            {
                let _ = provider_vault.remove(&replacement).await;
                return Err(ProviderError::new(ErrorKind::ReauthRequired).into());
            }
            let previous = SecretRef(std::mem::replace(
                &mut pending.credential_ref,
                replacement.0,
            ));
            if let Some(account_id) = account_id {
                pending.config.account_id = account_id;
            }
            pending.labels.account = labels.account.or(pending.labels.account.take());
            pending.labels.folder = labels.folder.or(pending.labels.folder.take());
            if let Err(error) = store.put_pending(&pending) {
                let _ = provider_vault
                    .remove(&SecretRef(pending.credential_ref.clone()))
                    .await;
                return Err(error.into());
            }
            let _ = provider_vault.remove(&previous).await;
            (pending, true)
        }
        Err(error) if error.kind == ErrorKind::NotFound => {
            if let Err(error) = cancel.check() {
                if let CredentialInput::Reference { reference, .. } = &credential {
                    let _ = provider_vault.remove(reference).await;
                }
                return Err(error.into());
            }
            let (credential_ref, account_id) = match credential {
                CredentialInput::Bytes(secret) => (
                    provider_vault.store(&secret.bytes).await?,
                    secret.account_id,
                ),
                CredentialInput::Reference {
                    reference,
                    account_id,
                } => (reference, account_id),
            };
            if let Some(account_id) = account_id {
                config.account_id = account_id;
            }
            let (repository_id, descriptor, key, recovery_key, provider_repository_id, create) =
                match preparation.request.mode {
                    ConnectionOpenMode::Create => (
                        uuid::Uuid::new_v4().to_string(),
                        None,
                        root_key().map_err(|_| ProviderError::new(ErrorKind::Transient))?,
                        recovery::generate_key()?,
                        None,
                        true,
                    ),
                    ConnectionOpenMode::Existing => {
                        let recovery_key = preparation
                            .recovery_key
                            .as_deref()
                            .ok_or_else(|| ProviderError::new(ErrorKind::ReauthRequired))?;
                        let discovered = async {
                            let provider = connection::provider_for(&config, dependencies.clone())?;
                            let (handle, capabilities) = provider
                                .open_repository(
                                    &config,
                                    &credential_ref,
                                    super::contract::OpenMode::Existing,
                                    cancel,
                                )
                                .await?;
                            let recovered = recovery::open_bootstrap(
                                &root,
                                provider.as_ref(),
                                &handle,
                                recovery_key,
                                cancel,
                            )
                            .await?;
                            if preparation
                                .expected_repository_id
                                .as_ref()
                                .is_some_and(|expected| {
                                    expected != &recovered.metadata.descriptor.repository_id
                                })
                            {
                                return Err(ProviderError::new(ErrorKind::RepositoryMismatch).into());
                            }
                            require_repository_strategy(
                                &capabilities,
                                recovered.metadata.descriptor.publication_strategy,
                            )?;
                            descriptor::read(
                                &root,
                                provider.as_ref(),
                                &handle,
                                &recovered.metadata.descriptor_locator,
                                &recovered.metadata.descriptor,
                                &recovered.key,
                                cancel,
                            )
                            .await?;
                            Ok::<_, ConnectionFailure>((recovered, handle.repository_id))
                        }
                        .await;
                        let (recovered, provider_repository_id) = match discovered {
                            Ok(value) => value,
                            Err(error) => {
                                let _ = provider_vault.remove(&credential_ref).await;
                                return Err(error);
                            }
                        };
                        (
                            recovered.metadata.descriptor.repository_id.clone(),
                            Some(recovered.metadata.descriptor),
                            recovered.key,
                            Zeroizing::new(recovery_key.to_owned()),
                            Some(provider_repository_id),
                            false,
                        )
                    }
                };
            let key_ref = match key_vault
                .store(&SecretBytes(Zeroizing::new(key.to_vec())))
                .await
            {
                Ok(reference) => reference,
                Err(error) => {
                    let _ = provider_vault.remove(&credential_ref).await;
                    return Err(error.into());
                }
            };
            let recovery_key_ref = match key_vault
                .store(&SecretBytes(Zeroizing::new(
                    recovery_key.as_bytes().to_vec(),
                )))
                .await
            {
                Ok(reference) => reference,
                Err(error) => {
                    let _ = key_vault.remove(&key_ref).await;
                    let _ = provider_vault.remove(&credential_ref).await;
                    return Err(error.into());
                }
            };
            let pending = PendingStoredConnection {
                id: connection_id.into(),
                config,
                repository_id,
                descriptor,
                create,
                provider_repository_id,
                credential_ref: credential_ref.0.clone(),
                root_key_ref: key_ref.0.clone(),
                recovery_key_ref: recovery_key_ref.0.clone(),
                created_at_ms: now_ms(),
                labels,
            };
            if let Err(error) = store.put_pending(&pending) {
                let _ = key_vault.remove(&key_ref).await;
                let _ = key_vault.remove(&recovery_key_ref).await;
                let _ = provider_vault.remove(&credential_ref).await;
                return Err(error.into());
            }
            (pending, false)
        }
        Err(error) => return Err(error.into()),
    };
    config = pending.config.clone();
    let credential_ref = SecretRef(pending.credential_ref.clone());
    let provider = connection::provider_for(&config, dependencies.clone())?;
    let open_mode = if pending.create {
        if resuming {
            super::contract::OpenMode::ResumeCreate
        } else {
            super::contract::OpenMode::Create
        }
    } else {
        super::contract::OpenMode::Existing
    };
    let creating = matches!(open_mode, super::contract::OpenMode::Create);
    let (handle, capabilities) = provider
        .open_repository(&config, &credential_ref, open_mode, cancel)
        .await.map_err(|error| occupied_location_error(creating, error))?;
    if pending
        .provider_repository_id
        .as_ref()
        .is_some_and(|expected| expected != &handle.repository_id)
    {
        return Err(ProviderError::new(ErrorKind::RepositoryMismatch).into());
    }
    // Nothing this attempt left behind can be promoted later, so it goes with
    // the refusal.
    if store
        .identity_holder(&pending.repository_id, &handle.connection_identity)?
        .is_some_and(|held| held != connection_id)
    {
        let _ = store.remove_pending(connection_id);
        let _ = key_vault
            .remove(&SecretRef(pending.root_key_ref.clone()))
            .await;
        let _ = key_vault
            .remove(&SecretRef(pending.recovery_key_ref.clone()))
            .await;
        let _ = provider_vault.remove(&credential_ref).await;
        return Err(ConnectionFailure::ALREADY_CONNECTED);
    }
    let mut updated = pending.clone();
    updated.provider_repository_id = Some(handle.repository_id.clone());
    if updated.descriptor.is_none() {
        let strategy = strategy_for_new_connection(preparation.request.purpose, &capabilities)?;
        updated.descriptor = Some(
            Descriptor::new(updated.repository_id.clone(), strategy)
                .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?,
        );
    }
    store.put_pending(&updated)?;

    let descriptor = updated
        .descriptor
        .as_ref()
        .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
    require_repository_strategy(&capabilities, descriptor.publication_strategy)?;
    let root_key = read_root_key(key_vault.as_ref(), &updated.root_key_ref).await?;
    let recovery_key = read_recovery_key(key_vault.as_ref(), &updated.recovery_key_ref).await?;
    let (descriptor_locator, format_level) = match preparation.request.mode {
        ConnectionOpenMode::Create => {
            let (locator, created) = ensure_create_descriptor(
                &root,
                &config.provider,
                provider.as_ref(),
                &handle,
                descriptor,
                &root_key,
                resuming,
                cancel,
            )
            .await?;
            recovery::publish_bootstrap(
                &root,
                provider.as_ref(),
                &handle,
                &recovery::BootstrapMetadata {
                    descriptor: descriptor.clone(),
                    descriptor_locator: locator.clone(),
                },
                &root_key,
                &recovery_key,
                cancel,
            )
            .await?;
            (locator, created.level())
        }
        ConnectionOpenMode::Existing => {
            let recovered =
                recovery::open_bootstrap(&root, provider.as_ref(), &handle, &recovery_key, cancel)
                    .await?;
            if recovered.metadata.descriptor != *descriptor || *recovered.key != *root_key {
                return Err(ProviderError::new(ErrorKind::RepositoryMismatch).into());
            }
            let current = descriptor::read(
                &root,
                provider.as_ref(),
                &handle,
                &recovered.metadata.descriptor_locator,
                descriptor,
                &root_key,
                cancel,
            )
            .await?;
            (recovered.metadata.descriptor_locator, current.level())
        }
    };
    cancel.check()?;
    let stored = store.promote_pending(connection_id, descriptor_locator, capabilities)?;
    store.record_format_level(connection_id, format_level)?;
    let recovery = if preparation.request.mode == ConnectionOpenMode::Create {
        Some(RecoveryKeyMaterial {
            key: recovery_key.to_string(),
        })
    } else {
        None
    };
    Ok(committed_result(&stored, preparation.request.mode, recovery))
}

/// A stored connection does not keep how it was opened, so the result carries the request's mode.
fn committed_result(
    stored: &StoredConnection,
    mode: ConnectionOpenMode,
    recovery: Option<RecoveryKeyMaterial>,
) -> ConnectionResult {
    let mut connection = connection::summary(stored);
    connection.mode = mode;
    ConnectionResult { connection, recovery }
}

async fn validate_sync_folder_before_commit(
    config: &mut super::contract::ConnectionConfig,
    purpose: ConnectionPurpose,
    credential: &CredentialInput,
    dependencies: &Dependencies,
    cancel: &Cancellation,
) -> Result<()> {
    if purpose != ConnectionPurpose::Sync {
        return Ok(());
    }
    if config.provider == "onedrive" {
        let reference = match credential {
            CredentialInput::Reference { reference, .. } => reference,
            _ => return Err(ProviderError::new(ErrorKind::Unsupported)),
        };
        let drive = config.location.get("driveId")
            .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
        let item = config.location.get("rootItemId")
            .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
        let folder = providers::onedrive::OneDrive::new(dependencies.clone()).inspect_setup_folder(
            config, reference, &config.account_id, drive, item, cancel,
        ).await?;
        config.location.insert("syncRootPath".into(), folder.sync_root_path()?.into());
    }
    connection::validate_sync_location(config)
}

pub(crate) async fn read_root_key(vault: &dyn SecretVault, reference: &str) -> Result<Zeroizing<[u8; 32]>> {
    let mut bytes = vault.read(&SecretRef(reference.into())).await.map_err(|error| {
        if error.kind == ErrorKind::ReauthRequired { ProviderError::new(ErrorKind::RepositoryKeyUnavailable) }
        else { error }
    })?;
    if bytes.0.len() != 32 {
        return Err(ProviderError::new(ErrorKind::RepositoryKeyUnavailable));
    }
    let mut key = Zeroizing::new([0; 32]);
    key.copy_from_slice(&bytes.0);
    bytes.0.zeroize();
    Ok(key)
}

async fn read_recovery_key(vault: &dyn SecretVault, reference: &str) -> Result<Zeroizing<String>> {
    let mut bytes = vault.read(&SecretRef(reference.into())).await?;
    let key =
        std::str::from_utf8(&bytes.0).map_err(|_| ProviderError::new(ErrorKind::ReauthRequired))?;
    risunest_external_storage_format::crypto::RecoveryKey::parse(key)
        .map_err(|_| ProviderError::new(ErrorKind::ReauthRequired))?;
    let result = Zeroizing::new(key.to_owned());
    bytes.0.zeroize();
    Ok(result)
}

pub(crate) async fn open_connected(
    app: &AppHandle,
    connection_id: &str,
) -> Result<ConnectedRepository> {
    open_connected_with_cancel(app, connection_id, &Cancellation::default()).await
}

pub(crate) async fn open_connected_with_cancel<R: tauri::Runtime>(
    app: &AppHandle<R>,
    connection_id: &str,
    cancel: &Cancellation,
) -> Result<ConnectedRepository> {
    cancel.check()?;
    let root = connection_root(app)?;
    let mut stored = ConnectionStore::open(&root)?.read(connection_id)?;
    let load = async {
        let dependencies = connection::dependencies_for_config(&root, &stored.config)?;
        let root_key = read_root_key(secrets::repository_key_vault(&root).as_ref(), &stored.root_key_ref).await?;
        let provider = connection::provider_for(&stored.config, dependencies.clone())?;
        Ok::<_, ProviderError>((dependencies, root_key, provider))
    };
    #[cfg(test)]
    let (dependencies, root_key, provider): (Dependencies, Zeroizing<[u8; 32]>, Arc<dyn Provider>) = match super::transfer_factory_tests::inputs(&root, connection_id) {
        Some(inputs) => { drop(load); (inputs.dependencies, inputs.root_key, inputs.provider) },
        None => load.await?,
    };
    #[cfg(not(test))]
    let (dependencies, root_key, provider) = load.await?;
    let provider = super::transfer_limit::wrap(&root, connection_id, provider)?;
    let (handle, capabilities) = provider
        .open_repository(
            &stored.config,
            &SecretRef(stored.credential_ref.clone()),
            super::contract::OpenMode::Existing,
            cancel,
        )
        .await?;
    if handle.repository_id != stored.provider_repository_id {
        return Err(ProviderError::new(ErrorKind::RepositoryMismatch));
    }
    stored
        .descriptor
        .validate()
        .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    require_repository_strategy(&capabilities, stored.descriptor.publication_strategy)?;
    stored.capabilities = capabilities;
    let current = descriptor::read(
        &root,
        provider.as_ref(),
        &handle,
        &stored.descriptor_locator,
        &stored.descriptor,
        &root_key,
        cancel,
    )
    .await?;
    ConnectionStore::open(&root)?.record_format_level(connection_id, current.level())?;
    Ok(ConnectedRepository {
        stored,
        provider,
        handle,
        dependencies,
        root_key,
    })
}

#[tauri::command]
pub(crate) fn external_storage_set_transfer_concurrency(app: AppHandle, connection_id: String, value: usize) -> Result<()> {
    logged("external_storage_set_transfer_concurrency", (|| {
        let cleanup_state = app.state::<ConnectionCommandState>();
        let _cleanup_guard = cleanup_state.admit()?;
        super::transfer_limit::set_limit(&connection_root(&app)?, &connection_id, value)
    })())
}

#[tauri::command]
pub(crate) fn external_storage_set_retention_policy(
    app: AppHandle,
    connection_id: String,
    policy: super::connection::RetentionPolicy,
) -> Result<()> {
    logged("external_storage_set_retention_policy", (|| {
        let cleanup_state = app.state::<ConnectionCommandState>();
        let _cleanup_guard = cleanup_state.admit()?;
        let root = connection_root(&app)?;
        let mut store = ConnectionStore::open(&root)?;
        store.set_retention_policy(&connection_id, policy)?;
        Ok(())
    })())
}

#[tauri::command]
pub(crate) async fn external_storage_remove_connection(
    app: AppHandle,
    connection_id: String,
) -> Result<()> {
    logged("external_storage_remove_connection", async move {
        let cleanup_state = app.state::<ConnectionCommandState>();
        let _cleanup_guard = cleanup_state.admit()?;
        let _removal = app
            .state::<super::job_store::JobCommandState>()
            .hold_connection_removal(&connection_id)?;
        let root = connection_root(&app)?;
        let file_jobs = app.state::<crate::native_file_jobs::NativeFileJobState>();
        let _permit = file_jobs
            .admission
            .file(true)
            .map_err(runtime::local_error)?;
        let mut pds = runtime::native_store(&app)?;
        pds.external_prepare_connection_removal(&connection_id)
            .map_err(runtime::local_error)?;
        drop(pds);
        super::lww_commands::invalidate_connection_cache(&connection_id).await?;
        runtime::end_removed_connection_jobs(&root, &connection_id)?;
        let store = ConnectionStore::open(&root)?;
        let stored = store.read(&connection_id)?;
        runtime::native_store(&app)?
            .external_lww_forget_target(&format!(
                "{}:{}",
                stored.descriptor.repository_id, stored.descriptor_locator.connection_identity
            ))
            .map_err(runtime::local_error)?;
        secrets::provider_vault(&root)
            .remove(&SecretRef(stored.credential_ref.clone()))
            .await?;
        secrets::repository_key_vault(&root)
            .remove(&SecretRef(stored.root_key_ref.clone()))
            .await?;
        secrets::repository_key_vault(&root)
            .remove(&SecretRef(stored.recovery_key_ref.clone()))
            .await?;
        super::transfer_limit::remove_connection(&root, &connection_id)?;
        // Sources left behind count as unavailable, because their connection is gone.
        let forgotten = runtime::native_store(&app).and_then(|pds| {
            pds.forget_external_connection_bodies(&connection_id).map_err(|error| runtime::local_error(error.code))
        });
        if let Err(error) = forgotten {
            crate::nlog!("warn", "Removed external connection sources were kept: {error}");
        }
        // A directory that stays behind is removed at the next startup.
        if let Err(error) = super::leftovers::remove_connection_directory(&root, &connection_id) {
            crate::nlog!("warn", "Removed external connection files were kept: {error}");
        }
        if let Err(error) = app.state::<crate::asset_repository::commands::DurableCasJobState>().sweep_settled_jobs(&app) {
            crate::nlog!("warn", "Removed external connection asset journals await cleanup: {error}");
        }
        Ok(())
    }.await)
}

#[tauri::command]
pub(crate) async fn external_storage_begin_connection_settings_export(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    connection_id: String,
) -> Result<ConnectionSettingsMaterial> {
    logged("external_storage_begin_connection_settings_export", async move {
        let _cleanup_guard = state.admit()?;
        let root = connection_root(&app)?;
        let stored = ConnectionStore::open(&root)?.read(&connection_id)?;
        let recovery_key = read_recovery_key(
            secrets::repository_key_vault(&root).as_ref(),
            &stored.recovery_key_ref,
        )
        .await?;
        let credential = if stored.config.oauth_profile.is_some() {
            None
        } else {
            Some(
                secrets::provider_vault(&root)
                    .read(&SecretRef(stored.credential_ref.clone()))
                    .await?,
            )
        };
        let bytes = recovery::export_connection_settings(
            &stored,
            &recovery_key,
            credential.as_ref().map(|value| value.0.as_slice()),
        )?;
        let transfer_id = uuid::Uuid::new_v4().to_string();
        let expires_at_ms = now_ms().saturating_add(PREPARATION_LIFETIME_MS);
        let payload = URL_SAFE_NO_PAD.encode(&bytes);
        let mut settings = lock(&state.connection_settings)?;
        settings.retain(|_, pending| pending.expires_at_ms > now_ms());
        settings.insert(
            transfer_id.clone(),
            PendingConnectionSettings {
                connection_id,
                expires_at_ms,
                payload: payload.clone(),
            },
        );
        Ok(ConnectionSettingsMaterial {
            transfer_id,
            expires_at_ms: expires_at_ms.to_string(),
            payload,
        })
    }.await)
}

#[tauri::command]
pub(crate) async fn external_storage_save_connection_settings_file(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    transfer_id: String,
) -> Result<()> {
    logged("external_storage_save_connection_settings_file", async move {
        let _cleanup_guard = state.admit()?;
        let pending = state.connection_settings_for_save(&transfer_id, now_ms())?;
        let selected = app
            .dialog()
            .file()
            .add_filter("RisuNest connection settings", &["rnconnection"])
            .set_file_name("risunest-connection.rnconnection")
            .blocking_save_file()
            .ok_or_else(|| ProviderError::new(ErrorKind::Cancelled))?;
        let encoded = pending.payload;
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        let mut file = app
            .fs()
            .open(selected.clone(), options)
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        file.write_all(encoded.as_bytes())
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        file.sync_all()
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        drop(file);
        let mut options = OpenOptions::new();
        options.read(true);
        let mut file = app
            .fs()
            .open(selected, options)
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        file.seek(std::io::SeekFrom::Start(0))
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        let mut verified = Vec::new();
        let max_encoded = risunest_external_storage_format::crypto::MAX_CONNECTION_SETTINGS_BYTES
            .saturating_add(2)
            / 3
            * 4;
        file.take((max_encoded + 1) as u64)
            .read_to_end(&mut verified)
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        if verified != encoded.as_bytes() {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let verified = URL_SAFE_NO_PAD
            .decode(&verified)
            .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
        let stored = ConnectionStore::open(&connection_root(&app)?)?.read(&pending.connection_id)?;
        let recovery_key = read_recovery_key(
            secrets::repository_key_vault(&connection_root(&app)?).as_ref(),
            &stored.recovery_key_ref,
        )
        .await?;
        let imported = recovery::import_connection_settings(&verified, &recovery_key).map_err(|error|
            if error.kind == ErrorKind::RecoveryKeyMismatch { ProviderError::new(ErrorKind::Corrupt) } else { error })?;
        if imported.repository_id != stored.descriptor.repository_id || imported.config != stored.config
        {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        Ok(())
    }.await)
}

#[tauri::command]
pub(crate) fn external_storage_prepare_connection_settings_import(
    state: State<'_, ConnectionCommandState>,
    request: PrepareConnectionSettingsImportRequest,
) -> Result<PreparedConnection> {
    logged("external_storage_prepare_connection_settings_import", (|| {
        let _cleanup_guard = state.admit()?;
        let max_encoded = risunest_external_storage_format::crypto::MAX_CONNECTION_SETTINGS_BYTES
            .saturating_add(2)
            / 3
            * 4;
        if request.payload.is_empty() || request.payload.len() > max_encoded {
            return Err(ProviderError::new(ErrorKind::Corrupt));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(request.payload.as_bytes())
            .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
        let recovery_key = Zeroizing::new(request.recovery_key);
        let imported = recovery::import_connection_settings(&bytes, &recovery_key)?;
        let mut acknowledgements = Vec::new();
        if imported.config.provider == "github_releases" {
            acknowledgements.push(GITHUB_ACKNOWLEDGEMENT.into());
        }
        let prepare = PrepareConnectionRequest {
            config: imported.config,
            mode: ConnectionOpenMode::Existing,
            purpose: ConnectionPurpose::Backup,
            recovery_key: Some(recovery_key.to_string()),
            acknowledgements,
        };
        let imported_credential = imported.credential.map(|bytes| ImportedCredential {
            bytes,
            account_id: imported.account_id,
        });
        insert_preparation(
            &state,
            prepare,
            Some(imported.repository_id),
            imported_credential,
            imported.folder_name,
            true,
        )
    })())
}

fn platform_key() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
    } else if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "ios") {
        "ios"
    } else {
        "linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_settings_save_retries_keep_the_same_payload_until_expiry() {
        let state = ConnectionCommandState::default();
        state.connection_settings.lock().unwrap().insert("transfer".into(), PendingConnectionSettings {
            connection_id: "connection".into(), expires_at_ms: 100, payload: "synthetic".repeat(3000),
        });
        let first = state.connection_settings_for_save("transfer", 1).unwrap();
        let retry = state.connection_settings_for_save("transfer", 2).unwrap();
        assert_eq!(first.payload, retry.payload);
        assert_eq!(first.connection_id, retry.connection_id);
        assert_eq!(state.connection_settings_for_save("transfer", 100).err().unwrap().kind, ErrorKind::Cancelled);
        assert!(state.connection_settings.lock().unwrap().is_empty());
    }

    /// The form reads the kind to tell a repository this device already holds
    /// from a provider failure, and a provider failure keeps its own shape.
    #[test]
    fn cleanup_fences_and_drains_connection_commands_before_reopening() {
        let state = ConnectionCommandState::default();
        let first = state.admit().unwrap();
        let second = state.admit().unwrap();
        let cancellation = Cancellation::default();
        state
            .authorization_cancellations
            .lock()
            .unwrap()
            .insert("synthetic".into(), cancellation.clone());
        state.begin_cleanup().unwrap();
        assert!(cancellation.check().is_err());
        assert!(state.admit().is_err());
        assert!(!state.cleanup_drained().unwrap());
        assert!(state.finish_cleanup().is_err());
        drop(first);
        assert!(!state.cleanup_drained().unwrap());
        drop(second);
        assert!(state.cleanup_drained().unwrap());
        assert!(state.authorization_cancellations.lock().unwrap().is_empty());
        state.finish_cleanup().unwrap();
        assert!(state.admit().is_ok());
    }

    #[test]
    fn a_refused_connection_reports_its_own_kind() {
        assert_eq!(
            serde_json::to_value(ConnectionFailure::ALREADY_CONNECTED).unwrap(),
            serde_json::json!({"kind":"alreadyConnected"})
        );
        assert_eq!(
            serde_json::to_value(ConnectionFailure::from(ProviderError::new(
                ErrorKind::Transient
            )))
            .unwrap(),
            serde_json::to_value(ProviderError::new(ErrorKind::Transient)).unwrap()
        );
    }

    #[test]
    fn incomplete_authorization_is_not_a_connection_or_consumed_error() {
        let pending = CompleteAuthorizationResult::Pending {
            authorization_pending: true,
            callback_rejected: false,
        };
        assert_eq!(
            serde_json::to_value(pending).unwrap(),
            serde_json::json!({"authorizationPending":true})
        );
        let rejected = CompleteAuthorizationResult::Pending {
            authorization_pending: true,
            callback_rejected: true,
        };
        assert_eq!(
            serde_json::to_value(rejected).unwrap(),
            serde_json::json!({"authorizationPending":true,"callbackRejected":true})
        );
    }

    #[test]
    fn only_google_authorization_accepts_a_client_secret() {
        let secret =
            authorization_client_secret("google_drive", Some("synthetic-secret".to_owned()))
                .unwrap()
                .unwrap();
        assert_eq!(secret.as_str(), "synthetic-secret");
        assert!(authorization_client_secret("google_drive", None)
            .unwrap()
            .is_none());
        assert_eq!(
            authorization_client_secret("onedrive", Some("synthetic-secret".to_owned()))
                .unwrap_err()
                .kind,
            ErrorKind::Unsupported
        );
    }

    #[test]
    fn cancelling_authorization_wakes_completion_after_ownership_is_taken() {
        let state = ConnectionCommandState::default();
        let cancel = Cancellation::default();
        state
            .authorization_cancellations
            .lock()
            .unwrap()
            .insert("synthetic-authorization".into(), cancel.clone());
        cancel_authorization(&state, "synthetic-authorization").unwrap();
        assert_eq!(cancel.check().unwrap_err().kind, ErrorKind::Cancelled);
        assert!(state.authorization_cancellations.lock().unwrap().is_empty());
    }

    #[test]
    fn restarted_create_reuses_the_authenticated_descriptor_without_uploading_again() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            // A repository for sync is created at this app's format level, a backup repository without one.
            for (strategy, level) in [(None, 1), (Some(super::super::contract::PublicationStrategy::Cas), 2)] {
                let first_root = tempfile::tempdir().unwrap();
                let restarted_root = tempfile::tempdir().unwrap();
                let provider = super::super::fake::FakeProvider::new(true);
                let handle = super::super::fake::repository();
                let descriptor = Descriptor::new(handle.repository_id.clone(), strategy).unwrap();
                let cancel = Cancellation::default();

                let first = ensure_create_descriptor(
                    first_root.path(),
                    "webdav",
                    &provider,
                    &handle,
                    &descriptor,
                    &[7; 32],
                    false,
                    &cancel,
                )
                .await
                .unwrap();
                assert_eq!(provider.upload_attempts(&descriptor.repository_id), 1);
                assert_eq!(first.1.level(), level);

                let resumed = ensure_create_descriptor(
                    restarted_root.path(),
                    "webdav",
                    &provider,
                    &handle,
                    &descriptor,
                    &[7; 32],
                    true,
                    &cancel,
                )
                .await
                .unwrap();
                assert_eq!(resumed, first);
                assert_eq!(provider.upload_attempts(&descriptor.repository_id), 1);
            }
        });
    }

    use std::collections::BTreeMap;

    #[test]
    fn onedrive_sync_commit_uses_actual_decoded_folder_path_even_when_supplied() {
        use super::super::{fake::{loopback_dependencies, MemoryVault}, wire_fixture::{Reply, WireServer}};
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let now = 1_700_000_000_000u64;
            let secret = serde_json::to_vec(&serde_json::json!({"refreshToken":"synthetic-refresh","accessToken":"synthetic-access","accessTokenExpiresAtMs":now+3_600_000})).unwrap();
            for allowed in [false, true] {
                let parent = if allowed {"Projects/%EC%83%88%20%ED%8F%B4%EB%8D%94".into()} else {"r".repeat(200)};
                let name = if allowed {"Allowed".into()} else {"r".repeat(68)};
                let body = serde_json::to_vec(&serde_json::json!({"id":"configured-root","name":name,"folder":{},"parentReference":{"driveId":"drive-1","path":format!("/drive/root:/{parent}")}})).unwrap();
                let server = WireServer::start(vec![Reply::Http {status:200,headers:vec![("Content-Type".into(),"application/json".into())],body}]);
                let dependencies = loopback_dependencies(MemoryVault::with("synthetic-secret", &secret), now).dependencies;
                let mut config = super::super::contract::ConnectionConfig {provider:"onedrive".into(), profile:Some("personal".into()), endpoint:server.url.to_string(), account_id:"account-1".into(),
                    location:BTreeMap::from([("accountType".into(),"personal".into()),("tenant".into(),"consumers".into()),("driveId".into(),"drive-1".into()),("rootItemId".into(),"configured-root".into()),("syncRootPath".into(),"forged-safe".into())]),
                    oauth_profile:Some(super::super::contract::OAuthProfile {project_id:"synthetic-app".into(),platform_client_ids:BTreeMap::new()})};
                let credential = CredentialInput::Reference {reference:SecretRef("synthetic-secret".into()),account_id:Some("account-1".into())};
                let result = validate_sync_folder_before_commit(&mut config, ConnectionPurpose::Sync, &credential, &dependencies, &Cancellation::default()).await;
                assert_eq!(server.requests.lock().unwrap().len(),1);
                if allowed {
                    result.unwrap();
                    assert_eq!(config.location["syncRootPath"], "Projects/새 폴더/Allowed");
                } else {assert_eq!(result.unwrap_err().kind,ErrorKind::Unsupported);}
            }
            let server = WireServer::start(vec![]);
            let dependencies = loopback_dependencies(MemoryVault::with("synthetic-secret", &secret), now).dependencies;
            let mut config = super::super::contract::ConnectionConfig {provider:"onedrive".into(),profile:Some("personal".into()),endpoint:server.url.to_string(),account_id:"account-1".into(),location:BTreeMap::from([("syncRootPath".into(),"supplied-backup-value".into())]),oauth_profile:None};
            let credential = CredentialInput::Reference {reference:SecretRef("synthetic-secret".into()),account_id:None};
            validate_sync_folder_before_commit(&mut config, ConnectionPurpose::Backup, &credential, &dependencies, &Cancellation::default()).await.unwrap();
            assert!(server.requests.lock().unwrap().is_empty());
            assert_eq!(config.location["syncRootPath"],"supplied-backup-value");
        });
    }

    #[test]
    fn existing_prepare_requires_a_well_formed_recovery_key() {
        let state = ConnectionCommandState::default();
        let request = PrepareConnectionRequest {
            config: super::super::contract::ConnectionConfig {
                provider: "webdav".into(),
                profile: None,
                endpoint: "https://synthetic.invalid".into(),
                account_id: "synthetic".into(),
                location: BTreeMap::from([("root".into(), "RisuNest".into())]),
                oauth_profile: None,
            },
            mode: ConnectionOpenMode::Existing,
            purpose: ConnectionPurpose::Backup,
            recovery_key: Some(recovery::generate_key().unwrap().to_string()),
            acknowledgements: Vec::new(),
        };
        let result = insert_preparation(&state, request, None, None, None, false).unwrap();
        assert!(!result.requires_recovery_key);
        assert!(result.capabilities.is_none());

        let mut invalid = state
            .preparations
            .lock()
            .unwrap()
            .values()
            .next()
            .unwrap()
            .request
            .clone();
        invalid.recovery_key = Some("not-a-recovery-key".into());
        assert!(insert_preparation(&state, invalid, None, None, None, false).is_err());
    }

    #[test]
    fn http_connections_prepare_and_list_only_for_user_configured_providers() {
        let state = ConnectionCommandState::default();
        let s3 = PrepareConnectionRequest {
            config: super::super::contract::ConnectionConfig {
                provider: "s3".into(),
                profile: Some("generic".into()),
                endpoint: "http://127.0.0.1:9000".into(),
                account_id: String::new(),
                location: BTreeMap::from([
                    ("bucket".into(), "synthetic-bucket".into()),
                    ("addressing".into(), "path".into()),
                ]),
                oauth_profile: None,
            },
            mode: ConnectionOpenMode::Create,
            purpose: ConnectionPurpose::Backup,
            recovery_key: None,
            acknowledgements: Vec::new(),
        };
        assert_eq!(insert_preparation(&state, s3, None, None, None, false).unwrap().endpoint.authority,
            "http://127.0.0.1:9000");

        let mut stored = StoredConnection {
            id: "http-webdav".into(),
            config: super::super::contract::ConnectionConfig {
                provider: "webdav".into(),
                profile: None,
                endpoint: "http://192.168.0.10:8080/dav".into(),
                account_id: "synthetic".into(),
                location: BTreeMap::from([("root".into(), "RisuNest".into())]),
                oauth_profile: None,
            },
            descriptor: Descriptor::new("synthetic-repository".into(), None).unwrap(),
            descriptor_locator: super::super::fake::locator(),
            provider_repository_id: "synthetic-provider-root".into(),
            credential_ref: "credential".into(),
            root_key_ref: "root-key".into(),
            recovery_key_ref: "recovery-key".into(),
            retention_policy: None, transfer_concurrency: None, labels: Default::default(),
            capabilities: super::super::fake::capabilities(false),
            created_at_ms: 1,
            verified_at_ms: 1,
            last_sync_at_ms: None,
            last_backup_at_ms: None,
        };
        assert!(summary(&stored).is_ok());
        stored.config.provider = "mybox".into();
        stored.config.account_id = String::new();
        stored.config.location = BTreeMap::from([("rootFolderName".into(), "RisuNest".into())]);
        stored.config.endpoint = "http://open-api.mybox.naver.com/v1".into();
        assert!(summary(&stored).is_err());
        stored.config.endpoint = "https://open-api.mybox.naver.com/v1".into();
        assert!(summary(&stored).is_ok());
    }

    /// Renewal accepts only the account the connection holds, so its sign-in page opens on that account.
    #[test]
    fn a_renewal_sign_in_selects_the_account_the_connection_holds() {
        let state = ConnectionCommandState::default();
        let stored = StoredConnection {
            id: "renewed".into(),
            config: super::super::contract::ConnectionConfig {
                provider: "webdav".into(),
                profile: None,
                endpoint: "https://dav.example.invalid/remote".into(),
                account_id: "synthetic".into(),
                location: BTreeMap::from([("root".into(), "RisuNest".into())]),
                oauth_profile: None,
            },
            descriptor: Descriptor::new("synthetic-repository".into(), None).unwrap(),
            descriptor_locator: super::super::fake::locator(),
            provider_repository_id: "synthetic-provider-root".into(),
            credential_ref: "credential".into(),
            root_key_ref: "root-key".into(),
            recovery_key_ref: "recovery-key".into(),
            retention_policy: None, transfer_concurrency: None,
            labels: ConnectionLabels::new(Some("synthetic@example.invalid".into()), None),
            capabilities: super::super::fake::capabilities(false),
            created_at_ms: 1,
            verified_at_ms: 1,
            last_sync_at_ms: None,
            last_backup_at_ms: None,
        };
        let sign_in = |preparation_id: &str| {
            let policy = super::super::auth::AuthorizationPolicy {
                authorize_url: url::Url::parse("https://accounts.example.invalid/authorize").unwrap(),
                client_id: "synthetic-public-client".into(),
                redirect_url: url::Url::parse("http://127.0.0.1:52001/oauth").unwrap(),
                scopes: vec!["synthetic.scope".into()],
                picker: false,
                login_hint: None,
            }
            .with_login_hint(authorization_login_hint(&state, preparation_id).unwrap());
            let (_, url) = super::super::auth::PendingAuthorization::start(policy).unwrap();
            url.query_pairs().filter(|(name, _)| name == "login_hint").map(|(_, value)| value.into_owned()).collect::<Vec<_>>()
        };

        let renewal = insert_renewal(&state, stored.clone(), "renewed".into()).unwrap();
        assert_eq!(sign_in(&renewal.preparation_id), vec!["synthetic@example.invalid".to_owned()]);

        let created = insert_preparation(&state, PrepareConnectionRequest {
            config: stored.config,
            mode: ConnectionOpenMode::Create,
            purpose: ConnectionPurpose::Backup,
            recovery_key: None,
            acknowledgements: Vec::new(),
        }, None, None, None, false).unwrap();
        assert!(sign_in(&created.preparation_id).is_empty());
    }

    /// The form sends S3 settings without an account, which the access key
    /// fills in only once the connection commits.
    #[test]
    fn an_http_s3_connection_commits_with_the_settings_the_form_sends() {
        use super::super::wire_fixture::{Reply, WireServer};
        let server = WireServer::start(vec![Reply::Http {
            status: 403,
            headers: vec![("Content-Type".into(), "application/xml".into())],
            body: b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>AccessDenied</Code></Error>".to_vec(),
        }]);
        let endpoint = server.url.origin().ascii_serialization();
        assert!(endpoint.starts_with("http://127.0.0.1:"));
        let state = ConnectionCommandState::default();
        let request = PrepareConnectionRequest {
            config: super::super::contract::ConnectionConfig {
                provider: "s3".into(),
                profile: Some("generic".into()),
                endpoint,
                account_id: String::new(),
                location: BTreeMap::from([
                    ("bucket".into(), "synthetic-bucket".into()),
                    ("prefix".into(), "risunest".into()),
                    ("region".into(), "us-east-1".into()),
                    ("addressing".into(), "path".into()),
                ]),
                oauth_profile: None,
            },
            mode: ConnectionOpenMode::Create,
            purpose: ConnectionPurpose::Backup,
            recovery_key: None,
            acknowledgements: Vec::new(),
        };
        let id = insert_preparation(&state, request, None, None, None, false).unwrap().preparation_id;
        let pending = take_preparation(&state, &id).unwrap();
        let secret = connection::encode_secret("s3", ProviderSecretInput::S3 {
            access_key_id: "AKIASYNTHETICEXAMPLE".into(),
            secret_access_key: "synthetic/secret/key".into(),
        }).unwrap();
        let root = tempfile::tempdir().unwrap();
        let result = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let result = commit_new_connection(
                root.path(), &id, &pending, CredentialInput::Bytes(secret), &Cancellation::default(),
            ).await;
            if let Ok(left) = ConnectionStore::open(root.path()).and_then(|store| store.pending(&id)) {
                let _ = secrets::provider_vault(root.path()).remove(&SecretRef(left.credential_ref)).await;
                let keys = secrets::repository_key_vault(root.path());
                let _ = keys.remove(&SecretRef(left.root_key_ref)).await;
                let _ = keys.remove(&SecretRef(left.recovery_key_ref)).await;
            }
            result
        });
        let Err(ConnectionFailure::Provider(error)) = result else { panic!("the refused listing must fail the commit") };
        assert_ne!(error.kind, ErrorKind::Unsupported);
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].headers.starts_with("GET /synthetic-bucket?"), "{}", requests[0].headers.lines().next().unwrap_or_default());
    }

    #[test]
    fn changing_an_s3_address_uses_the_new_http_origin_before_repository_verification() {
        use super::super::{fake::MemoryVault, wire_fixture::{Reply, WireServer}};
        for previous in ["http://previous.invalid:9000", "https://previous.invalid"] {
            let server = WireServer::start(vec![Reply::Http {
                status: 403,
                headers: vec![("Content-Type".into(), "application/xml".into())],
                body: b"<Error><Code>AccessDenied</Code></Error>".to_vec(),
            }]);
            let root = tempfile::tempdir().unwrap();
            let mut stored = renewal_fixture();
            stored.config.provider = "s3".into();
            stored.config.profile = Some("generic".into());
            stored.config.endpoint = previous.into();
            stored.config.location = BTreeMap::from([
                ("bucket".into(), "synthetic-bucket".into()),
                ("prefix".into(), "risunest".into()),
                ("region".into(), "us-east-1".into()),
                ("addressing".into(), "path".into()),
            ]);
            let secret = connection::encode_secret("s3", ProviderSecretInput::S3 {
                access_key_id: "AKIASYNTHETICEXAMPLE".into(), secret_access_key: "synthetic/secret/key".into(),
            }).unwrap();
            stored.config.account_id = secret.account_id.clone().unwrap();
            ConnectionStore::open(root.path()).unwrap().insert(&stored).unwrap();
            let endpoint = format!(" {} ", server.url.origin().ascii_serialization());
            for use_previous in [true, false] {
                let failure = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(
                    change_endpoint(root.path(), &stored, endpoint.clone(), |config| {
                        let config = if use_previous { &stored.config } else { config };
                        let mut dependencies = connection::dependencies_for_config(root.path(), config)?;
                        dependencies.vault = Arc::new(MemoryVault::with(&stored.credential_ref, &secret.bytes.0));
                        Ok(dependencies)
                    }, &[7; 32], &Cancellation::default()),
                ).err().expect("address replacement must fail before a repository is authenticated");
                if use_previous {
                    assert_eq!(failure.kind, ErrorKind::Unsupported);
                    assert!(server.requests.lock().unwrap().is_empty());
                } else {
                    assert_eq!(failure.http_status, Some(403), "{previous}: {failure:?}");
                    assert_ne!(failure.kind, ErrorKind::Unsupported);
                    assert_eq!(server.requests.lock().unwrap().len(), 1);
                }
            }
            let reread = ConnectionStore::open(root.path()).unwrap().read(&stored.id).unwrap();
            assert_eq!(serde_json::to_value(reread).unwrap(), serde_json::to_value(stored).unwrap());
        }
    }

    #[test]
    fn a_commit_saves_the_names_it_was_given_and_a_resumed_commit_keeps_unreported_ones() {
        use super::super::wire_fixture::{Reply, WireServer};
        let server = WireServer::start(vec![Reply::Http {
            status: 403,
            headers: vec![("Content-Type".into(), "application/xml".into())],
            body: b"<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>AccessDenied</Code></Error>".to_vec(),
        }]);
        let state = ConnectionCommandState::default();
        let request = PrepareConnectionRequest {
            config: super::super::contract::ConnectionConfig {
                provider: "s3".into(),
                profile: Some("generic".into()),
                endpoint: server.url.origin().ascii_serialization(),
                account_id: String::new(),
                location: BTreeMap::from([
                    ("bucket".into(), "synthetic-bucket".into()),
                    ("prefix".into(), "risunest".into()),
                    ("region".into(), "us-east-1".into()),
                    ("addressing".into(), "path".into()),
                ]),
                oauth_profile: None,
            },
            mode: ConnectionOpenMode::Create,
            purpose: ConnectionPurpose::Backup,
            recovery_key: None,
            acknowledgements: Vec::new(),
        };
        let id = insert_preparation(&state, request, None, None, Some("Imported folder".into()), false).unwrap().preparation_id;
        let mut pending = take_preparation(&state, &id).unwrap();
        pending.account_label = Some(" synthetic@example.invalid ".into());
        let secret = || connection::encode_secret("s3", ProviderSecretInput::S3 {
            access_key_id: "AKIASYNTHETICEXAMPLE".into(),
            secret_access_key: "synthetic/secret/key".into(),
        }).unwrap();
        let root = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let commit = |pending: &PendingPreparation| runtime.block_on(async {
            assert!(commit_new_connection(
                root.path(), &id, pending, CredentialInput::Bytes(secret()), &Cancellation::default(),
            ).await.is_err());
            ConnectionStore::open(root.path()).unwrap().pending(&id).unwrap().labels
        });
        assert_eq!(commit(&pending), ConnectionLabels::new(
            Some("synthetic@example.invalid".into()), Some("Imported folder".into())));
        pending.account_label = None;
        pending.selected_folder_name = Some("Selected folder".into());
        assert_eq!(commit(&pending), ConnectionLabels::new(
            Some("synthetic@example.invalid".into()), Some("Selected folder".into())));
        runtime.block_on(async {
            let left = ConnectionStore::open(root.path()).unwrap().pending(&id).unwrap();
            let _ = secrets::provider_vault(root.path()).remove(&SecretRef(left.credential_ref)).await;
            let keys = secrets::repository_key_vault(root.path());
            let _ = keys.remove(&SecretRef(left.root_key_ref)).await;
            let _ = keys.remove(&SecretRef(left.recovery_key_ref)).await;
        });
    }

    #[test]
    fn connection_settings_are_authenticated_before_endpoint_review() {
        let descriptor = Descriptor::new("synthetic-repository".into(), None).unwrap();
        let stored = StoredConnection {
            id: "source-device-only".into(),
            config: super::super::contract::ConnectionConfig {
                provider: "webdav".into(),
                profile: None,
                endpoint: "https://synthetic.invalid".into(),
                account_id: "synthetic".into(),
                location: BTreeMap::from([("root".into(), "RisuNest".into())]),
                oauth_profile: None,
            },
            descriptor,
            descriptor_locator: super::super::fake::locator(),
            provider_repository_id: "synthetic-provider-root".into(),
            credential_ref: "not-exported".into(),
            root_key_ref: "not-exported".into(),
            recovery_key_ref: "not-exported-recovery".into(),
            retention_policy: None, transfer_concurrency: None, labels: Default::default(),
            capabilities: super::super::fake::capabilities(false),
            created_at_ms: 1,
            verified_at_ms: 1,
            last_sync_at_ms: None,
            last_backup_at_ms: None,
        };
        let key = recovery::generate_key().unwrap();
        let exported =
            recovery::export_connection_settings(&stored, &key, Some(b"secret")).unwrap();
        let recovered = recovery::import_connection_settings(&exported, &key).unwrap();
        assert_eq!(recovered.config.endpoint, "https://synthetic.invalid");
        assert_eq!(recovered.credential.unwrap().as_slice(), b"secret");
        let mut damaged = exported;
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        assert!(recovery::import_connection_settings(&damaged, &key).is_err());
    }

    #[test]
    fn manual_recovery_can_authorize_and_bind_the_authenticated_account() {
        let state = ConnectionCommandState::default();
        let prepared = insert_preparation(
            &state,
            PrepareConnectionRequest {
                config: super::super::contract::ConnectionConfig {
                    provider: "google_drive".into(),
                    profile: Some("drive".into()),
                    endpoint: "https://www.googleapis.com".into(),
                    account_id: String::new(),
                    location: BTreeMap::from([
                        ("folderId".into(), "synthetic-folder".into()),
                        ("space".into(), "drive".into()),
                    ]),
                    oauth_profile: Some(super::super::contract::OAuthProfile {
                        project_id: "synthetic-project".into(),
                        platform_client_ids: BTreeMap::from([(
                            platform_key().into(),
                            "123-current.apps.googleusercontent.com".into(),
                        )]),
                    }),
                },
                mode: ConnectionOpenMode::Existing,
                purpose: ConnectionPurpose::Backup,
                recovery_key: Some(recovery::generate_key().unwrap().to_string()),
                acknowledgements: Vec::new(),
            },
            None,
            None,
            None,
            false,
        )
        .unwrap();
        let id = prepared.preparation_id;
        assert!(authorization_config(&state, &id, None).is_ok());
        let authenticated = "authenticated-account".to_string();
        {
            let mut preparations = state.preparations.lock().unwrap();
            let pending = preparations.get_mut(&id).unwrap();
            assert!(!pending.transferred);
            validate_authenticated_account(pending, Some(&authenticated)).unwrap();
            pending.request.config.account_id = authenticated.clone();
            validate_authenticated_account(pending, Some(&authenticated)).unwrap();
            assert_eq!(
                validate_authenticated_account(pending, Some(&"another-account".into()))
                    .unwrap_err()
                    .kind,
                ErrorKind::ReauthRequired,
            );
            pending.recovery_key = None;
        }
        assert_eq!(
            authorization_config(&state, &id, None).err().unwrap().kind,
            ErrorKind::ReauthRequired
        );
        state.preparations.lock().unwrap().get_mut(&id).unwrap().renewal = Some("existing-connection".into());
        let renewed = authorization_config(&state, &id, None).unwrap();
        assert_eq!(renewed.account_id, authenticated);
        assert_eq!(
            renewed.oauth_profile.unwrap().platform_client_ids.get(platform_key()).unwrap(),
            "123-current.apps.googleusercontent.com",
        );
        state
            .preparations
            .lock()
            .unwrap()
            .get_mut(&id)
            .unwrap()
            .expires_at_ms = 0;
        assert_eq!(
            authorization_config(&state, &id, None).err().unwrap().kind,
            ErrorKind::Cancelled
        );
    }

    #[test]
    fn oauth_client_platform_matches_the_compilation_target() {
        assert_eq!(platform_key(), std::env::consts::OS);
    }

    #[test]
    fn recovered_google_client_override_must_keep_the_authenticated_project() {
        let config = super::super::contract::ConnectionConfig {
            provider: "google_drive".into(),
            profile: Some("drive".into()),
            endpoint: "https://www.googleapis.com".into(),
            account_id: "synthetic-account".into(),
            location: BTreeMap::from([
                ("folderId".into(), "synthetic-folder".into()),
                ("space".into(), "drive".into()),
            ]),
            oauth_profile: Some(super::super::contract::OAuthProfile {
                project_id: "synthetic-project".into(),
                platform_client_ids: BTreeMap::from([(
                    "other-platform".into(),
                    "123-source.apps.googleusercontent.com".into(),
                )]),
            }),
        };
        let mut pending = PendingPreparation {
            request: PrepareConnectionRequest {
                config,
                mode: ConnectionOpenMode::Existing,
                purpose: ConnectionPurpose::Sync,
                recovery_key: Some(recovery::generate_key().unwrap().to_string()),
                acknowledgements: Vec::new(),
            },
            expires_at_ms: u64::MAX,
            recovery_key: Some(recovery::generate_key().unwrap()),
            expected_repository_id: Some("synthetic-repository".into()),
            imported_credential: None,
            bound_credential: None,
            selected_folder_name: None,
            imported_folder_name: None,
            account_label: None,
            transferred: true,
            renewal: None,
            login_hint: None,
        };

        assert!(apply_recovery_platform_client(
            &mut pending,
            Some("999-current.apps.googleusercontent.com".into())
        )
        .is_err());
        apply_recovery_platform_client(
            &mut pending,
            Some("123-current.apps.googleusercontent.com".into()),
        )
        .unwrap();
        apply_recovery_platform_client(
            &mut pending,
            Some("123-current.apps.googleusercontent.com".into()),
        )
        .unwrap();
        apply_recovery_platform_client(&mut pending, None).unwrap();
        assert!(apply_recovery_platform_client(
            &mut pending,
            Some("123-different.apps.googleusercontent.com".into()),
        )
        .is_err());
        pending.transferred = false;
        assert!(apply_recovery_platform_client(
            &mut pending,
            Some("123-current.apps.googleusercontent.com".into()),
        )
        .is_err());
        let oauth = pending.request.config.oauth_profile.unwrap();
        assert_eq!(oauth.project_id, "synthetic-project");
        assert_eq!(
            oauth.platform_client_ids.get(platform_key()).unwrap(),
            "123-current.apps.googleusercontent.com"
        );
    }

    #[test]
    fn recovered_onedrive_client_preserves_the_application_across_retries_and_renewal() {
        let state = ConnectionCommandState::default();
        let client_id = "12345678-1234-1234-1234-123456789012";
        let other_client_id = "87654321-4321-4321-4321-210987654321";
        let source_platform = if platform_key() == "windows" { "android" } else { "windows" };
        let prepared = insert_preparation(
            &state,
            PrepareConnectionRequest {
                config: super::super::contract::ConnectionConfig {
                    provider: "onedrive".into(), profile: None,
                    endpoint: "https://graph.microsoft.com/v1.0".into(),
                    account_id: "synthetic-account".into(),
                    location: BTreeMap::from([
                        ("accountType".into(), "personal".into()),
                        ("tenant".into(), "common".into()),
                        ("driveId".into(), "synthetic-drive".into()),
                        ("rootItemId".into(), "synthetic-folder".into()),
                        ("redirectUri".into(), "https://login.microsoftonline.com/common/oauth2/nativeclient".into()),
                    ]),
                    oauth_profile: Some(super::super::contract::OAuthProfile {
                        project_id: client_id.into(),
                        platform_client_ids: BTreeMap::from([(source_platform.into(), client_id.into())]),
                    }),
                },
                mode: ConnectionOpenMode::Existing, purpose: ConnectionPurpose::Backup,
                recovery_key: Some(recovery::generate_key().unwrap().to_string()),
                acknowledgements: vec![],
            },
            Some("synthetic-repository".into()), None, None, true,
        ).unwrap();
        assert!(prepared.requires_platform_o_auth_client);
        let id = prepared.preparation_id;
        assert_eq!(
            authorization_config(&state, &id, Some(other_client_id.into())).err().unwrap().kind,
            ErrorKind::Unsupported,
        );
        for _ in 0..2 {
            let config = authorization_config(&state, &id, Some(client_id.into())).unwrap();
            assert_eq!(config.account_id, "synthetic-account");
            assert_eq!(
                providers::onedrive::authorization_policy(&config, platform_key()).unwrap().client_id,
                client_id,
            );
        }
        {
            let mut preparations = state.preparations.lock().unwrap();
            let pending = preparations.get_mut(&id).unwrap();
            pending.recovery_key = None;
        }
        assert_eq!(
            authorization_config(&state, &id, None).err().unwrap().kind,
            ErrorKind::ReauthRequired,
        );
        {
            let mut preparations = state.preparations.lock().unwrap();
            let pending = preparations.get_mut(&id).unwrap();
            pending.renewal = Some("existing-connection".into());
            pending.transferred = false;
        }
        let config = authorization_config(&state, &id, None).unwrap();
        assert_eq!(config.account_id, "synthetic-account");
        assert_eq!(
            providers::onedrive::authorization_policy(&config, platform_key()).unwrap().client_id,
            client_id,
        );
    }

    fn renewal_fixture() -> StoredConnection {
        let handle = super::super::fake::repository();
        StoredConnection {
            id: "renewal".into(),
            config: super::super::contract::ConnectionConfig {
                provider: "webdav".into(), profile: None, endpoint: "https://synthetic.invalid".into(),
                account_id: "account".into(), location: [("root".into(), "RisuNest".into())].into(), oauth_profile: None,
            },
            descriptor: Descriptor::new("repository".into(), None).unwrap(),
            descriptor_locator: super::super::fake::locator(),
            provider_repository_id: handle.repository_id,
            credential_ref: "old".into(), root_key_ref: "root".into(), recovery_key_ref: "recovery".into(),
            capabilities: super::super::fake::capabilities(false), created_at_ms: 1,
            verified_at_ms: 1,
            last_sync_at_ms: None, last_backup_at_ms: None, retention_policy: None, transfer_concurrency: None, labels: Default::default(),
        }
    }

    #[test]
    fn a_committed_connection_reports_the_mode_it_was_opened_in() {
        let stored = renewal_fixture();
        let created = committed_result(&stored, ConnectionOpenMode::Create, Some(RecoveryKeyMaterial { key: "recovery".into() }));
        assert!(created.connection.mode == ConnectionOpenMode::Create);
        assert!(created.recovery.is_some());
        let opened = committed_result(&stored, ConnectionOpenMode::Existing, None);
        assert!(opened.connection.mode == ConnectionOpenMode::Existing);
        assert!(opened.recovery.is_none());
    }

    #[test]
    fn failed_renewal_keeps_the_original_secret_and_success_removes_it() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let vault = super::super::fake::MemoryVault::with("old", b"old-credential");
            for kind in [ErrorKind::PreconditionFailed, ErrorKind::Cancelled] {
                let replacement = vault.store(&SecretBytes(Zeroizing::new(b"replacement".to_vec()))).await.unwrap();
                let failed = complete_credential_renewal(&vault, &SecretRef("old".into()), &replacement,
                    Err(ProviderError::new(kind))).await;
                assert_eq!(failed.err().unwrap().kind, kind);
                assert_eq!(vault.contents("old").unwrap(), b"old-credential");
                assert!(vault.contents(&replacement.0).is_none());
            }
            let replacement = vault.store(&SecretBytes(Zeroizing::new(b"replacement".to_vec()))).await.unwrap();
            let mut updated = renewal_fixture();
            updated.credential_ref = replacement.0.clone();
            let success = complete_credential_renewal(&vault, &SecretRef("old".into()), &replacement, Ok(updated)).await.unwrap();
            assert_eq!(success.connection.id, "renewal");
            assert!(success.recovery.is_none());
            assert!(vault.contents("old").is_none());
            assert_eq!(vault.contents(&replacement.0).unwrap(), b"replacement");
        });
    }

    #[test]
    fn a_google_renewal_reuses_the_client_secret_saved_with_the_renewed_connection() {
        use super::super::{auth::AuthorizationCode, fake::{loopback_dependencies, MemoryVault}, wire_fixture::{Reply, WireServer}};
        let json = |body: serde_json::Value| Reply::Http {
            status: 200, headers: vec![("Content-Type".into(), "application/json".into())], body: serde_json::to_vec(&body).unwrap(),
        };
        let client = "123-current.apps.googleusercontent.com";
        let mut replies = vec![json(serde_json::json!({"access_token": "granted-access", "refresh_token": "granted-refresh", "expires_in": 3599}))];
        if cfg!(any(target_os = "android", target_os = "ios")) {
            replies.push(json(serde_json::json!({"aud": client, "azp": client, "scope": "https://www.googleapis.com/auth/drive.file"})));
        }
        replies.push(json(serde_json::json!({"user": {"permissionId": "permission-1"}})));
        let server = WireServer::start(replies);
        let root = tempfile::tempdir().unwrap();
        let mut stored = renewal_fixture();
        stored.config = super::super::contract::ConnectionConfig {
            provider: "google_drive".into(), profile: None, endpoint: server.url.to_string(), account_id: "permission-1".into(),
            location: BTreeMap::from([("folderId".into(), "folder-root".into())]),
            oauth_profile: Some(super::super::contract::OAuthProfile {
                project_id: "project".into(), platform_client_ids: BTreeMap::from([(platform_key().into(), client.into())]),
            }),
        };
        ConnectionStore::open(root.path()).unwrap().insert(&stored).unwrap();
        let dependencies = loopback_dependencies(
            MemoryVault::with(&stored.credential_ref, br#"{"refreshToken":"expired-refresh","clientSecret":"saved-client-secret"}"#),
            1_700_000_000_000,
        ).dependencies;
        let grant = AuthorizationCode {
            code: SecretBytes(Zeroizing::new(b"synthetic-code".to_vec())),
            verifier: SecretBytes(Zeroizing::new(b"synthetic-verifier".to_vec())),
            client_id: client.into(),
            redirect_url: url::Url::parse("http://127.0.0.1:52001/oauth").unwrap(),
            picked_file_id: None,
        };
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let credential = exchange_oauth_credential(root.path(), &dependencies, &stored.config, &grant, None,
                Some(&stored.id), &Cancellation::default()).await.unwrap();
            let body = String::from_utf8(server.requests.lock().unwrap()[0].body.clone()).unwrap();
            assert!(body.contains("client_secret=saved-client-secret"), "{body}");
            let saved = dependencies.vault.read(&credential.reference).await.unwrap();
            assert!(String::from_utf8(saved.0.to_vec()).unwrap().contains("\"clientSecret\":\"saved-client-secret\""));
        });
    }

    #[test]
    fn renewal_requires_the_same_oauth_account_and_repository_but_allows_key_rotation() {
        let mut stored = renewal_fixture();
        assert!(require_renewal_account(&stored.config, Some("rotated-key-fingerprint")).is_ok());
        stored.config.oauth_profile = Some(super::super::contract::OAuthProfile {
            project_id: "project".into(), platform_client_ids: Default::default(),
        });
        assert!(require_renewal_account(&stored.config, Some("account")).is_ok());
        assert_eq!(require_renewal_account(&stored.config, Some("another-account")).unwrap_err().kind, ErrorKind::ReauthRequired);
        let mut handle = super::super::fake::repository();
        assert!(require_renewal_repository(&stored, &handle).is_ok());
        handle.repository_id = "another-repository".into();
        assert_eq!(require_renewal_repository(&stored, &handle).unwrap_err().kind, ErrorKind::PreconditionFailed);
    }

    #[test]
    fn missing_repository_key_requires_unlocking_instead_of_provider_sign_in() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let vault = super::super::fake::MemoryVault::default();
            assert_eq!(read_root_key(&vault, "missing").await.unwrap_err().kind, ErrorKind::RepositoryKeyUnavailable);
        });
    }

    #[test]
    fn only_initial_create_uses_the_occupied_location_explanation() {
        let error = ProviderError::new(ErrorKind::PreconditionFailed);
        assert_eq!(occupied_location_error(true, error.clone()).kind, ErrorKind::LocationOccupied);
        assert_eq!(occupied_location_error(false, error).kind, ErrorKind::PreconditionFailed);
        assert_eq!(occupied_location_error(true, ProviderError::new(ErrorKind::Corrupt)).kind, ErrorKind::Corrupt);
    }

}

#[tauri::command]
pub(crate) fn external_storage_validate_sync_root(config: super::contract::ConnectionConfig) -> Result<()> {
    logged("external_storage_validate_sync_root", match config.provider.as_str() {
        "google_drive" if !config.location.contains_key("folderId") => Ok(()),
        "onedrive" if !config.location.contains_key("syncRootPath") => Ok(()),
        _ => connection::validate_sync_location(&config),
    })
}
