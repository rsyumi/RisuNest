//! Native external-storage connection, authorization and recovery commands.
//! Network operations never run while the library PDS mutex is held.
use super::{
    auth::{SecretBytes, SecretVault},
    connection::{self, *},
    connection_store::{ConnectionStore, PendingStoredConnection, StoredConnection},
    contract::{
        Cancellation, ErrorKind, Provider, ProviderError, RepositoryHandle, Result, SecretRef,
    },
    descriptor,
    providers::{self, Dependencies},
    recovery,
    runtime, secrets,
};
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
    transferred: bool,
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

struct PendingConnectionSettings {
    connection_id: String,
    expires_at_ms: u64,
    bytes: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct ConnectionCommandState {
    preparations: Mutex<HashMap<String, PendingPreparation>>,
    authorizations: Mutex<HashMap<String, PendingAuthorization>>,
    authorization_cancellations: Mutex<HashMap<String, Cancellation>>,
    connection_settings: Mutex<HashMap<String, PendingConnectionSettings>>,
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

type ConnectResult<T> = std::result::Result<T, ConnectionFailure>;

#[tauri::command]
pub(crate) fn external_storage_cancel_authorization(
    state: State<'_, ConnectionCommandState>,
    authorization_id: String,
) -> Result<()> {
    cancel_authorization(&state, &authorization_id)
}

fn cancel_authorization(
    state: &ConnectionCommandState,
    authorization_id: &str,
) -> Result<()> {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    qr_payload: Option<String>,
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
    transferred: bool,
) -> Result<PreparedConnection> {
    if transferred
        && request.config.oauth_profile.is_some()
        && request.config.account_id.is_empty()
    {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let endpoint = connection::validate_preparation(&request)?;
    let recovery_key = request
        .recovery_key
        .take()
        .map(|value| {
            risunest_external_storage_format::crypto::RecoveryKey::parse(&value)
                .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
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
            transferred,
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
    let missing = !profile.platform_client_ids.contains_key(platform_key());
    if !missing {
        return if supplied.is_none() {
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
    {
        return Err(ProviderError::new(ErrorKind::ReauthRequired));
    }
    apply_recovery_platform_client(preparation, current_platform_client_id)?;
    Ok(preparation.request.config.clone())
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

fn google_project_number(client_id: &str) -> Option<&str> {
    let (project, suffix) = client_id.split_once('-')?;
    (!project.is_empty()
        && project.bytes().all(|byte| byte.is_ascii_digit())
        && !suffix.is_empty()
        && client_id.ends_with(".apps.googleusercontent.com"))
    .then_some(project)
}

pub(crate) fn connection_root(app: &AppHandle) -> Result<PathBuf> {
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
    require_repository_strategy(&connection.capabilities, connection.descriptor.publication_strategy)?;
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
    insert_preparation(&state, request, None, None, false)
}

#[tauri::command]
pub(crate) async fn external_storage_commit_connection(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: CommitConnectionRequest,
) -> ConnectResult<ConnectionResult> {
    let preparation_id = request.preparation_id;
    let pending = take_preparation(&state, &preparation_id)?;
    if pending.request.config.oauth_profile.is_some() {
        restore_preparation(&state, preparation_id, pending);
        return Err(ProviderError::new(ErrorKind::Unsupported).into());
    }
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
    let cancel = Cancellation::default();
    match commit_preparation(
        &app,
        &preparation_id,
        &pending,
        CredentialInput::Bytes(secret),
        &cancel,
    )
    .await
    {
        Ok(result) => Ok(result),
        Err(error) => {
            restore_preparation(&state, preparation_id, pending);
            Err(error)
        }
    }
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub(crate) async fn external_storage_begin_authorization(
    state: State<'_, ConnectionCommandState>,
    request: BeginAuthorizationRequest,
) -> Result<PendingAuthorizationSummary> {
    let BeginAuthorizationRequest {
        preparation_id,
        current_platform_client_id,
    } = request;
    let mut exchange_config = {
        authorization_config(&state, &preparation_id, current_platform_client_id)?
    };
    let provider = exchange_config.provider.clone();
    let (flow, authorization_url) = match provider.as_str() {
        "google_drive" => {
            super::oauth::LoopbackAuthorization::start(|redirect| {
                providers::google_drive::auth::native_authorization_policy(
                    &exchange_config,
                    redirect,
                )
            })
            .await?
        }
        "onedrive" => {
            super::oauth::LoopbackAuthorization::start(|redirect| {
                exchange_config
                    .location
                    .insert("redirectUri".into(), redirect.to_string());
                providers::onedrive::authorization_policy(&exchange_config, platform_key())
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
}

#[cfg(target_os = "ios")]
#[tauri::command]
pub(crate) async fn external_storage_begin_authorization(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: BeginAuthorizationRequest,
) -> Result<PendingAuthorizationSummary> {
    let BeginAuthorizationRequest {
        preparation_id,
        current_platform_client_id,
    } = request;
    let config = {
        authorization_config(&state, &preparation_id, current_platform_client_id)?
    };
    let mut exchange_config = config;
    let flow = match exchange_config.provider.as_str() {
        "google_drive" => {
            let (policy, callback_scheme) =
                providers::google_drive::auth::ios_authorization_policy(&exchange_config)?;
            super::oauth::IosWebAuthenticationAuthorization::start(
                policy,
                callback_scheme,
                true,
            )?
        }
        "onedrive" => {
            exchange_config.location.insert(
                "redirectUri".into(),
                providers::onedrive::IOS_REDIRECT_URI.into(),
            );
            let policy = providers::onedrive::authorization_policy(&exchange_config, "ios")?;
            let callback_scheme = policy.redirect_url.scheme().to_owned();
            super::oauth::IosWebAuthenticationAuthorization::start(
                policy,
                callback_scheme,
                false,
            )?
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
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) async fn external_storage_begin_authorization(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: BeginAuthorizationRequest,
) -> Result<PendingAuthorizationSummary> {
    let BeginAuthorizationRequest {
        preparation_id,
        current_platform_client_id,
    } = request;
    let config = {
        authorization_config(&state, &preparation_id, current_platform_client_id)?
    };
    let authorization_id = uuid::Uuid::new_v4().to_string();
    let expires_at_ms = now_ms().saturating_add(PREPARATION_LIFETIME_MS);
    let (authorization, authorization_url, authorization_state) = match config.provider.as_str() {
        "google_drive" => {
            let policy = providers::google_drive::auth::android_web_authorization_policy(&config)?;
            let (flow, url) = super::oauth::android_google_web_authorization(policy)?;
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
            let policy = providers::onedrive::authorization_policy(&exchange_config, "android")?;
            let (flow, url) = super::oauth::android_redirect_authorization(policy)?;
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
}

#[cfg(not(any(target_os = "android", target_os = "ios")))]
#[tauri::command]
pub(crate) async fn external_storage_complete_authorization(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: CompleteAuthorizationRequest,
) -> ConnectResult<CompleteAuthorizationResult> {
    let client_secret = request.client_secret.map(Zeroizing::new);
    if client_secret.is_some() {
        return Err(ProviderError::new(ErrorKind::Unsupported).into());
    }
    if let Some(mut redirect_url) = request.redirect_url {
        redirect_url.zeroize();
        return Err(ProviderError::new(ErrorKind::Unsupported).into());
    }
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
    let credential_result: Result<(SecretRef, Option<String>)> = async {
        let root = connection_root(&app)?;
        let dependencies = connection::dependencies(&root)?;
        let grant = authorization.flow.wait(&cancel).await?;
        match pending.request.config.provider.as_str() {
            "google_drive" => {
                let authorized = providers::google_drive::auth::exchange_authorization_code(
                    &dependencies,
                    &authorization.exchange_config,
                    &grant,
                    None,
                    &cancel,
                )
                .await?;
                Ok((
                    dependencies.vault.store(&authorized.secret).await?,
                    Some(authorized.account_id),
                ))
            }
            "onedrive" => {
                let provider = providers::onedrive::OneDrive::new(dependencies.clone());
                let authorized = provider
                    .exchange_authorization_code(
                        &authorization.exchange_config,
                        &grant,
                        &cancel,
                    )
                    .await?;
                Ok((authorized.secret, Some(authorized.account_id)))
            }
            _ => Err(ProviderError::new(ErrorKind::Unsupported)),
        }
    }
    .await;
    let (credential, account_id) = match credential_result {
        Ok(value) => value,
        Err(error) => {
            lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
            restore_preparation(&state, authorization.preparation_id, pending);
            return Err(error.into());
        }
    };
    let committed = commit_preparation(
        &app,
        &authorization.preparation_id,
        &pending,
        CredentialInput::Reference {
            reference: credential,
            account_id,
        },
        &cancel,
    )
    .await;
    lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
    match committed {
        Ok(result) => Ok(CompleteAuthorizationResult::Connected(result)),
        Err(error) => {
            restore_preparation(&state, authorization.preparation_id, pending);
            Err(error)
        }
    }
}

#[cfg(target_os = "ios")]
#[tauri::command]
pub(crate) async fn external_storage_complete_authorization(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: CompleteAuthorizationRequest,
) -> ConnectResult<CompleteAuthorizationResult> {
    if request.redirect_url.is_some() || request.client_secret.is_some() {
        return Err(ProviderError::new(ErrorKind::Unsupported).into());
    }
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
    let credential_result: Result<(SecretRef, Option<String>)> = async {
        let root = connection_root(&app)?;
        let dependencies = connection::dependencies(&root)?;
        match authorization.exchange_config.provider.as_str() {
            "google_drive" => {
                let authorized = providers::google_drive::auth::exchange_authorization_code(
                    &dependencies,
                    &authorization.exchange_config,
                    &authorization.grant,
                    None,
                    &cancel,
                )
                .await?;
                Ok((
                    dependencies.vault.store(&authorized.secret).await?,
                    Some(authorized.account_id),
                ))
            }
            "onedrive" => {
                let provider = providers::onedrive::OneDrive::new(dependencies);
                let authorized = provider
                    .exchange_authorization_code(
                        &authorization.exchange_config,
                        &authorization.grant,
                        &cancel,
                    )
                    .await?;
                Ok((authorized.secret, Some(authorized.account_id)))
            }
            _ => Err(ProviderError::new(ErrorKind::Unsupported)),
        }
    }
    .await;
    let (credential, account_id) = match credential_result {
        Ok(value) => value,
        Err(error) => {
            lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
            restore_preparation(&state, authorization.preparation_id, pending);
            return Err(error.into());
        }
    };
    let committed = commit_preparation(
        &app,
        &authorization.preparation_id,
        &pending,
        CredentialInput::Reference {
            reference: credential,
            account_id,
        },
        &cancel,
    )
    .await;
    lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
    match committed {
        Ok(result) => Ok(CompleteAuthorizationResult::Connected(result)),
        Err(error) => {
            restore_preparation(&state, authorization.preparation_id, pending);
            Err(error)
        }
    }
}

#[cfg(target_os = "android")]
#[tauri::command]
pub(crate) async fn external_storage_complete_authorization(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: CompleteAuthorizationRequest,
) -> ConnectResult<CompleteAuthorizationResult> {
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
    let credential_result: Result<(SecretRef, Option<String>)> = async {
        let root = connection_root(&app)?;
        let dependencies = connection::dependencies(&root)?;
        match authorization {
            PendingAuthorization::Google {
                exchange_config, ..
            } => {
                let authorized = providers::google_drive::auth::exchange_authorization_code(
                    &dependencies,
                    &exchange_config,
                    &grant,
                    client_secret,
                    &cancel,
                )
                .await?;
                Ok((
                    dependencies.vault.store(&authorized.secret).await?,
                    Some(authorized.account_id),
                ))
            }
            PendingAuthorization::OneDrive {
                exchange_config, ..
            } => {
                let provider = providers::onedrive::OneDrive::new(dependencies);
                let authorized = provider
                    .exchange_authorization_code(&exchange_config, &grant, &cancel)
                    .await?;
                Ok((authorized.secret, Some(authorized.account_id)))
            }
        }
    }
    .await;
    let (credential, account_id) = match credential_result {
        Ok(value) => value,
        Err(error) => {
            lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
            restore_preparation(&state, preparation_id, pending);
            return Err(error.into());
        }
    };
    let committed = commit_preparation(
        &app,
        &preparation_id,
        &pending,
        CredentialInput::Reference {
            reference: credential,
            account_id,
        },
        &cancel,
    )
    .await;
    lock(&state.authorization_cancellations)?.remove(&request.authorization_id);
    match committed {
        Ok(result) => Ok(CompleteAuthorizationResult::Connected(result)),
        Err(error) => {
            restore_preparation(&state, preparation_id, pending);
            Err(error)
        }
    }
}

enum CredentialInput {
    Bytes(EncodedProviderSecret),
    Reference {
        reference: SecretRef,
        account_id: Option<String>,
    },
}

async fn ensure_create_descriptor(
    root: &std::path::Path,
    provider: &dyn Provider,
    handle: &RepositoryHandle,
    descriptor: &Descriptor,
    root_key: &[u8; 32],
    resuming: bool,
    cancel: &Cancellation,
) -> Result<super::contract::RemoteLocator> {
    let resumed = if resuming {
        descriptor::resume_existing(root, provider, handle, descriptor, root_key, cancel).await?
    } else {
        None
    };
    let locator = match resumed {
        Some(locator) => locator,
        None => descriptor::upload(root, provider, handle, descriptor, root_key, cancel).await?,
    };
    descriptor::read(
        root,
        provider,
        handle,
        &locator,
        descriptor,
        root_key,
        cancel,
    )
    .await?;
    Ok(locator)
}

async fn commit_preparation(
    app: &AppHandle,
    connection_id: &str,
    preparation: &PendingPreparation,
    credential: CredentialInput,
    cancel: &Cancellation,
) -> ConnectResult<ConnectionResult> {
    let root = connection_root(app)?;
    let dependencies = connection::dependencies(&root)?;
    let provider_vault = dependencies.vault.clone();
    let key_vault = secrets::repository_key_vault(&root);
    let mut config = preparation.request.config.clone();
    let mut store = ConnectionStore::open(&root)?;
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
    let effective_connection_id = if preparation.request.mode == ConnectionOpenMode::Create
        && store.pending(connection_id).is_err_and(|error| error.kind == ErrorKind::NotFound)
    {
        store
            .pending_create_for(&config, preparation.request.capture_policy)?
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
            if account_id.as_ref().is_some_and(|account_id| {
                account_id != &pending.config.account_id
            }) {
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
            cancel.check()?;
            let (credential_ref, account_id) = match credential {
                CredentialInput::Bytes(secret) => (
                    provider_vault.store(&secret.bytes).await?,
                    secret.account_id,
                ),
                CredentialInput::Reference { reference, account_id } => (reference, account_id),
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
                                return Err(ProviderError::new(ErrorKind::Corrupt).into());
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
            let key_ref = match key_vault.store(&SecretBytes(Zeroizing::new(key.to_vec()))).await {
                Ok(reference) => reference,
                Err(error) => {
                    let _ = provider_vault.remove(&credential_ref).await;
                    return Err(error.into());
                }
            };
            let recovery_key_ref = match key_vault
                .store(&SecretBytes(Zeroizing::new(recovery_key.as_bytes().to_vec())))
                .await
            {
                Ok(reference) => reference,
                Err(error) => {
                    let _ = key_vault.remove(&key_ref).await;
                    let _ = provider_vault.remove(&credential_ref).await;
                    return Err(error.into());
                }
            };
            let capture_policy = descriptor
                .as_ref()
                .map(|value| {
                    value
                        .publication_strategy
                        .is_none()
                        .then(CapturePolicy::default)
                })
                .unwrap_or(preparation.request.capture_policy);
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
                capture_policy,
                created_at_ms: now_ms(),
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
    let (handle, capabilities) = provider
        .open_repository(&config, &credential_ref, open_mode, cancel)
        .await?;
    if pending
        .provider_repository_id
        .as_ref()
        .is_some_and(|expected| expected != &handle.repository_id)
    {
        return Err(ProviderError::new(ErrorKind::Corrupt).into());
    }
    // Nothing this attempt left behind can be promoted later, so it goes with
    // the refusal.
    if store
        .identity_holder(&handle.connection_identity)?
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

    let descriptor = updated.descriptor.as_ref().ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
    require_repository_strategy(&capabilities, descriptor.publication_strategy)?;
    let root_key = read_root_key(key_vault.as_ref(), &updated.root_key_ref).await?;
    let recovery_key = read_recovery_key(key_vault.as_ref(), &updated.recovery_key_ref).await?;
    let descriptor_locator = match preparation.request.mode {
        ConnectionOpenMode::Create => {
            let locator = ensure_create_descriptor(
                &root,
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
            locator
        }
        ConnectionOpenMode::Existing => {
            let recovered = recovery::open_bootstrap(
                &root,
                provider.as_ref(),
                &handle,
                &recovery_key,
                cancel,
            )
            .await?;
            if recovered.metadata.descriptor != *descriptor || *recovered.key != *root_key {
                return Err(ProviderError::new(ErrorKind::Corrupt).into());
            }
            descriptor::read(
                &root,
                provider.as_ref(),
                &handle,
                &recovered.metadata.descriptor_locator,
                descriptor,
                &root_key,
                cancel,
            )
            .await?;
            recovered.metadata.descriptor_locator
        }
    };
    cancel.check()?;
    let stored = store.promote_pending(connection_id, descriptor_locator, capabilities)?;
    let recovery = if preparation.request.mode == ConnectionOpenMode::Create {
        Some(RecoveryKeyMaterial {
            key: recovery_key.to_string(),
        })
    } else {
        None
    };
    Ok(ConnectionResult {
        connection: connection::summary(&stored),
        recovery,
    })
}

async fn read_root_key(vault: &dyn SecretVault, reference: &str) -> Result<Zeroizing<[u8; 32]>> {
    let mut bytes = vault.read(&SecretRef(reference.into())).await?;
    if bytes.0.len() != 32 {
        return Err(ProviderError::new(ErrorKind::ReauthRequired));
    }
    let mut key = Zeroizing::new([0; 32]);
    key.copy_from_slice(&bytes.0);
    bytes.0.zeroize();
    Ok(key)
}

async fn read_recovery_key(
    vault: &dyn SecretVault,
    reference: &str,
) -> Result<Zeroizing<String>> {
    let mut bytes = vault.read(&SecretRef(reference.into())).await?;
    let key = std::str::from_utf8(&bytes.0)
        .map_err(|_| ProviderError::new(ErrorKind::ReauthRequired))?;
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
    let root = connection_root(app)?;
    let mut stored = ConnectionStore::open(&root)?.read(connection_id)?;
    let dependencies = connection::dependencies(&root)?;
    let root_key = read_root_key(
        secrets::repository_key_vault(&root).as_ref(),
        &stored.root_key_ref,
    )
    .await?;
    let provider = connection::provider_for(&stored.config, dependencies.clone())?;
    let cancel = Cancellation::default();
    let (handle, capabilities) = provider
        .open_repository(
            &stored.config,
            &SecretRef(stored.credential_ref.clone()),
            super::contract::OpenMode::Existing,
            &cancel,
        )
        .await?;
    if handle.repository_id != stored.provider_repository_id {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    stored
        .descriptor
        .validate()
        .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    require_repository_strategy(&capabilities, stored.descriptor.publication_strategy)?;
    stored.capabilities = capabilities;
    descriptor::read(
        &root,
        provider.as_ref(),
        &handle,
        &stored.descriptor_locator,
        &stored.descriptor,
        &root_key,
        &cancel,
    )
    .await?;
    Ok(ConnectedRepository {
        stored,
        provider,
        handle,
        dependencies,
        root_key,
    })
}

/// Changes what a backup connection captures. The new policy applies to work
/// started afterwards; a job already running keeps the one it fixed.
#[tauri::command]
pub(crate) fn external_storage_set_capture_policy(
    app: AppHandle,
    connection_id: String,
    policy: super::connection::CapturePolicy,
) -> Result<()> {
    let root = connection_root(&app)?;
    let mut store = ConnectionStore::open(&root)?;
    store.set_capture_policy(&connection_id, policy)?;
    Ok(())
}

/// Changes how much of what this device backed up a connection keeps. The new
/// policy applies to cleanups started afterwards.
#[tauri::command]
pub(crate) fn external_storage_set_retention_policy(
    app: AppHandle,
    connection_id: String,
    policy: super::connection::RetentionPolicy,
) -> Result<()> {
    let root = connection_root(&app)?;
    let mut store = ConnectionStore::open(&root)?;
    store.set_retention_policy(&connection_id, policy)?;
    Ok(())
}

#[tauri::command]
pub(crate) async fn external_storage_remove_connection(
    app: AppHandle,
    connection_id: String,
) -> Result<()> {
    runtime::require_connection_idle(&app, &connection_id).await?;
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
    let mut store = ConnectionStore::open(&root)?;
    let stored = store.read(&connection_id)?;
    secrets::provider_vault(&root)
        .remove(&SecretRef(stored.credential_ref.clone()))
        .await?;
    secrets::repository_key_vault(&root)
        .remove(&SecretRef(stored.root_key_ref.clone()))
        .await?;
    secrets::repository_key_vault(&root)
        .remove(&SecretRef(stored.recovery_key_ref.clone()))
        .await?;
    store.remove(&connection_id)?;
    Ok(())
}

#[tauri::command]
pub(crate) async fn external_storage_begin_connection_settings_export(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    connection_id: String,
) -> Result<ConnectionSettingsMaterial> {
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
    let encoded = URL_SAFE_NO_PAD.encode(&bytes);
    let qr_payload = (encoded.len() <= 2_300).then_some(encoded);
    lock(&state.connection_settings)?.insert(
        transfer_id.clone(),
        PendingConnectionSettings {
            connection_id,
            expires_at_ms,
            bytes,
        },
    );
    Ok(ConnectionSettingsMaterial {
        transfer_id,
        expires_at_ms: expires_at_ms.to_string(),
        qr_payload,
    })
}

#[tauri::command]
pub(crate) async fn external_storage_save_connection_settings_file(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    transfer_id: String,
) -> Result<()> {
    let pending = lock(&state.connection_settings)?
        .remove(&transfer_id)
        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
    if pending.expires_at_ms <= now_ms() {
        return Err(ProviderError::new(ErrorKind::Cancelled));
    }
    let selected = app
        .dialog()
        .file()
        .add_filter("RisuNest connection settings", &["rnconnection"])
        .set_file_name("risunest-connection.rnconnection")
        .blocking_save_file()
        .ok_or_else(|| ProviderError::new(ErrorKind::Cancelled))?;
    let encoded = URL_SAFE_NO_PAD.encode(&pending.bytes);
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
    let max_encoded =
        risunest_external_storage_format::crypto::MAX_CONNECTION_SETTINGS_BYTES
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
    let imported = recovery::import_connection_settings(&verified, &recovery_key)?;
    if imported.repository_id != stored.descriptor.repository_id || imported.config != stored.config {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn external_storage_prepare_connection_settings_import(
    state: State<'_, ConnectionCommandState>,
    request: PrepareConnectionSettingsImportRequest,
) -> Result<PreparedConnection> {
    let max_encoded =
        risunest_external_storage_format::crypto::MAX_CONNECTION_SETTINGS_BYTES
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
        capture_policy: Some(super::connection::CapturePolicy::default()),
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
        true,
    )
}

fn platform_key() -> &'static str {
    if cfg!(target_os = "windows") {
        "windows"
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

    /// The form reads the kind to tell a repository this device already holds
    /// from a provider failure, and a provider failure keeps its own shape.
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
            let first_root = tempfile::tempdir().unwrap();
            let restarted_root = tempfile::tempdir().unwrap();
            let provider = super::super::fake::FakeProvider::new(true);
            let handle = super::super::fake::repository();
            let descriptor = Descriptor::new(handle.repository_id.clone(), None).unwrap();
            let cancel = Cancellation::default();

            let first = ensure_create_descriptor(
                first_root.path(),
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

            let resumed = ensure_create_descriptor(
                restarted_root.path(),
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
        });
    }

    use std::collections::BTreeMap;

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
            capture_policy: Some(super::connection::CapturePolicy::default()),
            recovery_key: Some(recovery::generate_key().unwrap().to_string()),
            acknowledgements: Vec::new(),
        };
        let result = insert_preparation(&state, request, None, None, false).unwrap();
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
        assert!(insert_preparation(&state, invalid, None, None, false).is_err());
    }

    #[test]
    fn connection_settings_are_authenticated_before_endpoint_review() {
        let descriptor = Descriptor::new("synthetic-repository".into(), None,
        )
        .unwrap();
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
            capture_policy: None,
            retention_policy: None,
            capabilities: super::super::fake::capabilities(false),
            created_at_ms: 1,
            last_sync_at_ms: None,
            last_backup_at_ms: None,
        };
        let key = recovery::generate_key().unwrap();
        let exported = recovery::export_connection_settings(&stored, &key, Some(b"secret")).unwrap();
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
                capture_policy: Some(CapturePolicy::default()),
                recovery_key: Some(recovery::generate_key().unwrap().to_string()),
                acknowledgements: Vec::new(),
            },
            None,
            None,
            false,
        ).unwrap();
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
                    .unwrap_err().kind,
                ErrorKind::ReauthRequired,
            );
            pending.recovery_key = None;
        }
        assert_eq!(authorization_config(&state, &id, None).err().unwrap().kind, ErrorKind::ReauthRequired);
        state.preparations.lock().unwrap().get_mut(&id).unwrap().expires_at_ms = 0;
        assert_eq!(authorization_config(&state, &id, None).err().unwrap().kind, ErrorKind::Cancelled);
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
                capture_policy: None,
                recovery_key: Some(recovery::generate_key().unwrap().to_string()),
                acknowledgements: Vec::new(),
            },
            expires_at_ms: u64::MAX,
            recovery_key: Some(recovery::generate_key().unwrap()),
            expected_repository_id: Some("synthetic-repository".into()),
            imported_credential: None,
            transferred: true,
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
        let oauth = pending.request.config.oauth_profile.unwrap();
        assert_eq!(oauth.project_id, "synthetic-project");
        assert_eq!(
            oauth.platform_client_ids.get(platform_key()).unwrap(),
            "123-current.apps.googleusercontent.com"
        );
    }
}
