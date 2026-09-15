//! Native external-storage connection, authorization and recovery commands.
//! Network operations never run while the library PDS mutex is held.
use super::{
    auth::{SecretBytes, SecretVault},
    capabilities::Evidence,
    connection::{self, *},
    connection_store::{ConnectionStore, PendingStoredConnection, StoredConnection},
    contract::{
        Cancellation, ErrorKind, Provider, ProviderError, RepositoryHandle, Result, SecretRef,
    },
    descriptor,
    providers::{self, Dependencies},
    quota_profiles,
    recovery::{self, ImportedRecovery},
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
    recovery: Option<ImportedRecovery>,
}

#[cfg(not(target_os = "android"))]
struct PendingAuthorization {
    preparation_id: String,
    expires_at_ms: u64,
    flow: super::oauth::LoopbackAuthorization,
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

struct PendingRecovery {
    connection_id: String,
    expires_at_ms: u64,
    bytes: Vec<u8>,
    code: Zeroizing<String>,
}

#[derive(Default)]
pub(crate) struct ConnectionCommandState {
    preparations: Mutex<HashMap<String, PendingPreparation>>,
    authorizations: Mutex<HashMap<String, PendingAuthorization>>,
    recoveries: Mutex<HashMap<String, PendingRecovery>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CommitConnectionRequest {
    preparation_id: String,
    secret: ProviderSecretInput,
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

#[tauri::command]
pub(crate) fn external_storage_cancel_authorization(
    state: State<'_, ConnectionCommandState>,
    authorization_id: String,
) -> Result<()> {
    lock(&state.authorizations)?.remove(&authorization_id);
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrepareRecoveryImportRequest {
    payload: String,
    code: String,
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
pub(crate) struct RecoveryMaterial {
    recovery_id: String,
    expires_at_ms: String,
    code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    qr_payload: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectionResult {
    connection: ConnectionSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovery: Option<RecoveryMaterial>,
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
    request: PrepareConnectionRequest,
    recovery: Option<ImportedRecovery>,
) -> Result<PreparedConnection> {
    if recovery.is_some()
        && request.config.oauth_profile.is_some()
        && request.config.account_id.is_empty()
    {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let endpoint = connection::validate_preparation(&request)?;
    let preparation_id = uuid::Uuid::new_v4().to_string();
    let expires_at_ms = now_ms().saturating_add(PREPARATION_LIFETIME_MS);
    let missing_platform_client = recovery.is_some()
        && request
            .config
            .oauth_profile
            .as_ref()
            .is_some_and(|profile| !profile.platform_client_ids.contains_key(platform_key()));
    let result = PreparedConnection {
        preparation_id: preparation_id.clone(),
        expires_at_ms: expires_at_ms.to_string(),
        endpoint,
        capabilities: predicted_capabilities(request.publication_strategy),
        requires_o_auth: request.config.oauth_profile.is_some(),
        requires_recovery_key: request.mode == ConnectionOpenMode::Existing && recovery.is_none(),
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
            recovery,
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
    if preparation.recovery.is_none() {
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

pub(crate) fn budget(app: &AppHandle) -> Result<super::durable_quota::DurableBudget> {
    super::durable_quota::DurableBudget::open(&connection_root(app)?.join("account-quota.sqlite"))
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
    match connection.descriptor.publication_strategy {
        Some(strategy) => connection.capabilities.require(strategy)?,
        None if connection.capabilities.immutable_create != Evidence::Unverified
            && connection.capabilities.direct_complete_read != Evidence::Unverified => {}
        None => return Err(ProviderError::new(ErrorKind::Corrupt)),
    }
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
    insert_preparation(&state, request, None)
}

#[tauri::command]
pub(crate) async fn external_storage_commit_connection(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: CommitConnectionRequest,
) -> Result<ConnectionResult> {
    let preparation_id = request.preparation_id;
    let pending = take_preparation(&state, &preparation_id)?;
    if pending.request.mode == ConnectionOpenMode::Existing && pending.recovery.is_none() {
        restore_preparation(&state, preparation_id, pending);
        return Err(ProviderError::new(ErrorKind::ReauthRequired));
    }
    if pending.request.config.oauth_profile.is_some() {
        restore_preparation(&state, preparation_id, pending);
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let secret = connection::encode_secret(&pending.request.config.provider, request.secret)?;
    match commit_preparation(
        &app,
        &preparation_id,
        &pending,
        CredentialInput::Bytes(secret),
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

#[cfg(not(target_os = "android"))]
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
        let mut pending = lock(&state.preparations)?;
        let preparation = pending
            .get_mut(&preparation_id)
            .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        if preparation.expires_at_ms <= now_ms() {
            return Err(ProviderError::new(ErrorKind::Cancelled));
        }
        if preparation.request.mode == ConnectionOpenMode::Existing
            && preparation.recovery.is_none()
        {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        apply_recovery_platform_client(preparation, current_platform_client_id)?;
        preparation.request.config.clone()
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
    Ok(PendingAuthorizationSummary {
        authorization_id,
        authorization_url: Some(authorization_url.to_string()),
        expires_at_ms: expires_at_ms.to_string(),
        state: "browser-required",
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
        let mut pending = lock(&state.preparations)?;
        let preparation = pending
            .get_mut(&preparation_id)
            .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        if preparation.expires_at_ms <= now_ms() {
            return Err(ProviderError::new(ErrorKind::Cancelled));
        }
        if preparation.request.mode == ConnectionOpenMode::Existing
            && preparation.recovery.is_none()
        {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        apply_recovery_platform_client(preparation, current_platform_client_id)?;
        preparation.request.config.clone()
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
    Ok(PendingAuthorizationSummary {
        authorization_id,
        authorization_url,
        expires_at_ms: expires_at_ms.to_string(),
        state: authorization_state,
    })
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
pub(crate) async fn external_storage_complete_authorization(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    request: CompleteAuthorizationRequest,
) -> Result<CompleteAuthorizationResult> {
    let client_secret = request.client_secret.map(Zeroizing::new);
    if client_secret.is_some() {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    if let Some(mut redirect_url) = request.redirect_url {
        redirect_url.zeroize();
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let authorization = lock(&state.authorizations)?
        .remove(&request.authorization_id)
        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
    if authorization.expires_at_ms <= now_ms() {
        return Err(ProviderError::new(ErrorKind::Cancelled));
    }
    let pending = take_preparation(&state, &authorization.preparation_id)?;
    let credential_result: Result<(SecretRef, Option<String>)> = async {
        let root = connection_root(&app)?;
        let (dependencies, _) = connection::dependencies(&root)?;
        let grant = authorization.flow.wait(&Cancellation::default()).await?;
        match pending.request.config.provider.as_str() {
            "google_drive" => {
                let authorized = providers::google_drive::auth::exchange_authorization_code(
                    &dependencies,
                    &authorization.exchange_config,
                    &grant,
                    None,
                    &Cancellation::default(),
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
                        &Cancellation::default(),
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
            restore_preparation(&state, authorization.preparation_id, pending);
            return Err(error);
        }
    };
    match commit_preparation(
        &app,
        &authorization.preparation_id,
        &pending,
        CredentialInput::Reference {
            reference: credential,
            account_id,
        },
    )
    .await
    {
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
) -> Result<CompleteAuthorizationResult> {
    let redirect_url = request.redirect_url.map(Zeroizing::new);
    let client_secret = request.client_secret.map(Zeroizing::new);
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
                    return Err(ProviderError::new(ErrorKind::Unsupported));
                }
                (*expires_at_ms, flow)
            }
        };
        if expires_at_ms <= now_ms() {
            authorizations.remove(&request.authorization_id);
            return Err(ProviderError::new(ErrorKind::Cancelled));
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
                return Err(error);
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
    let pending = take_preparation(&state, &preparation_id)?;
    let credential_result: Result<(SecretRef, Option<String>)> = async {
        let root = connection_root(&app)?;
        let (dependencies, _) = connection::dependencies(&root)?;
        match authorization {
            PendingAuthorization::Google {
                exchange_config, ..
            } => {
                let authorized = providers::google_drive::auth::exchange_authorization_code(
                    &dependencies,
                    &exchange_config,
                    &grant,
                    client_secret,
                    &Cancellation::default(),
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
                    .exchange_authorization_code(&exchange_config, &grant, &Cancellation::default())
                    .await?;
                Ok((authorized.secret, Some(authorized.account_id)))
            }
        }
    }
    .await;
    let (credential, account_id) = match credential_result {
        Ok(value) => value,
        Err(error) => {
            restore_preparation(&state, preparation_id, pending);
            return Err(error);
        }
    };
    match commit_preparation(
        &app,
        &preparation_id,
        &pending,
        CredentialInput::Reference {
            reference: credential,
            account_id,
        },
    )
    .await
    {
        Ok(result) => Ok(CompleteAuthorizationResult::Connected(result)),
        Err(error) => {
            restore_preparation(&state, preparation_id, pending);
            Err(error)
        }
    }
}

enum CredentialInput {
    Bytes(SecretBytes),
    Reference {
        reference: SecretRef,
        account_id: Option<String>,
    },
}

async fn commit_preparation(
    app: &AppHandle,
    connection_id: &str,
    preparation: &PendingPreparation,
    credential: CredentialInput,
) -> Result<ConnectionResult> {
    let root = connection_root(app)?;
    let (dependencies, durable_budget) = connection::dependencies(&root)?;
    let provider_vault = dependencies.vault.clone();
    let key_vault = secrets::repository_key_vault(&root);
    let mut config = preparation.request.config.clone();
    let mut store = ConnectionStore::open(&root)?;
    let (pending, resuming) = match store.pending(connection_id) {
        Ok(mut pending) => {
            let (replacement, account_id) = match credential {
                CredentialInput::Bytes(bytes) => (provider_vault.store(&bytes).await?, None),
                CredentialInput::Reference {
                    reference,
                    account_id,
                } => (reference, account_id),
            };
            if account_id.as_ref().is_some_and(|account_id| {
                preparation.request.mode == ConnectionOpenMode::Existing
                    && account_id != &pending.config.account_id
            }) {
                let _ = provider_vault.remove(&replacement).await;
                return Err(ProviderError::new(ErrorKind::ReauthRequired));
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
                return Err(error);
            }
            let _ = provider_vault.remove(&previous).await;
            (pending, true)
        }
        Err(error) if error.kind == ErrorKind::NotFound => {
            let (descriptor, key) = match preparation.request.mode {
                ConnectionOpenMode::Create => (
                    Descriptor::new(
                        uuid::Uuid::new_v4().to_string(),
                        preparation.request.scope.clone(),
                        preparation.request.publication_strategy.descriptor(),
                    )
                    .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?,
                    root_key().map_err(|_| ProviderError::new(ErrorKind::Transient))?,
                ),
                ConnectionOpenMode::Existing => {
                    let recovery = preparation
                        .recovery
                        .as_ref()
                        .ok_or_else(|| ProviderError::new(ErrorKind::ReauthRequired))?;
                    (recovery.metadata.descriptor.clone(), recovery.key.clone())
                }
            };
            let (credential_ref, account_id) = match credential {
                CredentialInput::Bytes(bytes) => (provider_vault.store(&bytes).await?, None),
                CredentialInput::Reference {
                    reference,
                    account_id,
                } => (reference, account_id),
            };
            if account_id.as_ref().is_some_and(|account_id| {
                preparation.request.mode == ConnectionOpenMode::Existing
                    && account_id != &config.account_id
            }) {
                let _ = provider_vault.remove(&credential_ref).await;
                return Err(ProviderError::new(ErrorKind::ReauthRequired));
            }
            if let Some(account_id) = account_id {
                config.account_id = account_id;
            }
            let key_ref = match key_vault
                .store(&SecretBytes(Zeroizing::new(key.to_vec())))
                .await
            {
                Ok(reference) => reference,
                Err(error) => {
                    let _ = provider_vault.remove(&credential_ref).await;
                    return Err(error);
                }
            };
            let pending = PendingStoredConnection {
                id: connection_id.into(),
                config,
                descriptor,
                provider_repository_id: preparation
                    .recovery
                    .as_ref()
                    .map(|recovery| recovery.metadata.provider_repository_id.clone()),
                credential_ref: credential_ref.0.clone(),
                root_key_ref: key_ref.0.clone(),
                created_at_ms: now_ms(),
            };
            if let Err(error) = store.put_pending(&pending) {
                let _ = key_vault.remove(&key_ref).await;
                let _ = provider_vault.remove(&credential_ref).await;
                return Err(error);
            }
            (pending, false)
        }
        Err(error) => return Err(error),
    };
    config = pending.config.clone();
    let credential_ref = SecretRef(pending.credential_ref.clone());
    let root_key = read_root_key(key_vault.as_ref(), &pending.root_key_ref).await?;
    let provider = connection::provider_for(&config, dependencies.clone())?;
    let cancel = Cancellation::default();
    let open_mode = if resuming {
        super::contract::OpenMode::Existing
    } else {
        preparation.request.mode.into()
    };
    let opened = provider
        .open_repository(&config, &credential_ref, open_mode, &cancel)
        .await;
    let (handle, mut capabilities) = match opened {
        Ok(value) => value,
        Err(error)
            if resuming
                && preparation.request.mode == ConnectionOpenMode::Create
                && error.kind == ErrorKind::NotFound =>
        {
            provider
                .open_repository(
                    &config,
                    &credential_ref,
                    super::contract::OpenMode::Create,
                    &cancel,
                )
                .await?
        }
        Err(error) => return Err(error),
    };
    if pending
        .provider_repository_id
        .as_ref()
        .is_some_and(|expected| expected != &handle.repository_id)
    {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let mut updated = pending.clone();
    updated.provider_repository_id = Some(handle.repository_id.clone());
    store.put_pending(&updated)?;

    if config.provider == "webdav"
        && preparation.request.publication_strategy == ConnectionStrategy::Cas
    {
        let probe = providers::webdav::probe_conditional_writes(
            &dependencies,
            &config,
            &credential_ref,
            &cancel,
        )
        .await?;
        store.remember_discovery(connection_id, "webdav-conditional-write-probe", &probe)?;
        if probe.create_if_absent && probe.exact_version_update && probe.strong_version_token {
            capabilities.atomic_create_head = Evidence::Synthetic;
            capabilities.conditional_head_update = Evidence::Synthetic;
        }
    }
    match preparation.request.publication_strategy.descriptor() {
        Some(strategy) => capabilities.require(strategy)?,
        None if capabilities.immutable_create != Evidence::Unverified
            && capabilities.direct_complete_read != Evidence::Unverified => {}
        None => return Err(ProviderError::new(ErrorKind::Unsupported)),
    }
    quota_profiles::configure_connection_budget(
        &durable_budget,
        &config.provider,
        config.profile.as_deref(),
        provider.as_ref(),
        &handle,
        &[],
        now_ms(),
    )?;
    let descriptor_locator = match preparation.request.mode {
        ConnectionOpenMode::Create => {
            let locator = descriptor::upload(
                &root,
                provider.as_ref(),
                &handle,
                &updated.descriptor,
                &root_key,
                &cancel,
            )
            .await?;
            descriptor::read(
                &root,
                provider.as_ref(),
                &handle,
                &locator,
                &updated.descriptor,
                &root_key,
                &cancel,
            )
            .await?;
            locator
        }
        ConnectionOpenMode::Existing => {
            let recovery = preparation
                .recovery
                .as_ref()
                .ok_or_else(|| ProviderError::new(ErrorKind::ReauthRequired))?;
            descriptor::read(
                &root,
                provider.as_ref(),
                &handle,
                &recovery.metadata.descriptor_locator,
                &updated.descriptor,
                &root_key,
                &cancel,
            )
            .await?;
            recovery.metadata.descriptor_locator.clone()
        }
    };
    let stored = store.promote_pending(connection_id, descriptor_locator, capabilities)?;
    let recovery = if preparation.request.mode == ConnectionOpenMode::Create {
        Some(create_recovery_material(app, &stored, &root_key)?)
    } else {
        None
    };
    Ok(ConnectionResult {
        connection: summary(&stored)?,
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

pub(crate) async fn open_connected(
    app: &AppHandle,
    connection_id: &str,
) -> Result<ConnectedRepository> {
    let root = connection_root(app)?;
    let stored = ConnectionStore::open(&root)?.read(connection_id)?;
    let (dependencies, durable_budget) = connection::dependencies(&root)?;
    let root_key = read_root_key(
        secrets::repository_key_vault(&root).as_ref(),
        &stored.root_key_ref,
    )
    .await?;
    let provider = connection::provider_for(&stored.config, dependencies.clone())?;
    let cancel = Cancellation::default();
    let (handle, _capabilities) = provider
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
    match stored.descriptor.publication_strategy {
        Some(strategy) => stored.capabilities.require(strategy)?,
        None if stored.capabilities.immutable_create != Evidence::Unverified
            && stored.capabilities.direct_complete_read != Evidence::Unverified => {}
        None => return Err(ProviderError::new(ErrorKind::Unsupported)),
    }
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
    quota_profiles::configure_connection_budget(
        &durable_budget,
        &stored.config.provider,
        stored.config.profile.as_deref(),
        provider.as_ref(),
        &handle,
        &[],
        now_ms(),
    )?;
    Ok(ConnectedRepository {
        stored,
        provider,
        handle,
        dependencies,
        root_key,
    })
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
    store.remove(&connection_id)?;
    Ok(())
}

fn create_recovery_material(
    app: &AppHandle,
    stored: &StoredConnection,
    key: &[u8; 32],
) -> Result<RecoveryMaterial> {
    create_recovery_material_with_state(&app.state::<ConnectionCommandState>(), stored, key)
}

fn create_recovery_material_with_state(
    state: &ConnectionCommandState,
    stored: &StoredConnection,
    key: &[u8; 32],
) -> Result<RecoveryMaterial> {
    let exported = recovery::export(stored, key)?;
    let recovery_id = uuid::Uuid::new_v4().to_string();
    let expires_at_ms = now_ms().saturating_add(PREPARATION_LIFETIME_MS);
    let encoded = URL_SAFE_NO_PAD.encode(&exported.bytes);
    // Version 40 QR byte mode at the UI's default M correction level holds
    // 2,331 bytes. Keep a little room for library framing differences.
    let qr_payload = (encoded.len() <= 2_300).then_some(encoded);
    let material = RecoveryMaterial {
        recovery_id: recovery_id.clone(),
        expires_at_ms: expires_at_ms.to_string(),
        code: exported.code.to_string(),
        qr_payload,
    };
    lock(&state.recoveries)?.insert(
        recovery_id,
        PendingRecovery {
            connection_id: stored.id.clone(),
            expires_at_ms,
            bytes: exported.bytes,
            code: exported.code,
        },
    );
    Ok(material)
}

#[tauri::command]
pub(crate) async fn external_storage_begin_recovery_export(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    connection_id: String,
) -> Result<RecoveryMaterial> {
    let root = connection_root(&app)?;
    let stored = ConnectionStore::open(&root)?.read(&connection_id)?;
    let key = read_root_key(
        secrets::repository_key_vault(&root).as_ref(),
        &stored.root_key_ref,
    )
    .await?;
    create_recovery_material_with_state(&state, &stored, &key)
}

#[tauri::command]
pub(crate) async fn external_storage_save_recovery_file(
    app: AppHandle,
    state: State<'_, ConnectionCommandState>,
    recovery_id: String,
) -> Result<()> {
    let pending = lock(&state.recoveries)?
        .remove(&recovery_id)
        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
    if pending.expires_at_ms <= now_ms() {
        return Err(ProviderError::new(ErrorKind::Cancelled));
    }
    let selected = app
        .dialog()
        .file()
        .add_filter("RisuNest recovery", &["rnrecovery"])
        .set_file_name("risunest-key-connection.rnrecovery")
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
        risunest_external_storage_format::crypto::MAX_RECOVERY_BYTES.saturating_add(2) / 3 * 4;
    file.take((max_encoded + 1) as u64)
        .read_to_end(&mut verified)
        .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    if verified != encoded.as_bytes() {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let verified = URL_SAFE_NO_PAD
        .decode(&verified)
        .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    let imported = recovery::import(&verified, &pending.code)?;
    let stored = ConnectionStore::open(&connection_root(&app)?)?.read(&pending.connection_id)?;
    if imported.metadata.descriptor != stored.descriptor
        || imported.metadata.provider_repository_id != stored.provider_repository_id
    {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(())
}

#[tauri::command]
pub(crate) fn external_storage_prepare_recovery_import(
    state: State<'_, ConnectionCommandState>,
    request: PrepareRecoveryImportRequest,
) -> Result<PreparedConnection> {
    let max_encoded =
        risunest_external_storage_format::crypto::MAX_RECOVERY_BYTES.saturating_add(2) / 3 * 4;
    if request.payload.is_empty() || request.payload.len() > max_encoded {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(request.payload.as_bytes())
        .map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    let code = Zeroizing::new(request.code);
    let imported = recovery::import(&bytes, &code)?;
    let strategy = match imported.metadata.descriptor.publication_strategy {
        Some(super::contract::PublicationStrategy::Cas) => ConnectionStrategy::Cas,
        Some(super::contract::PublicationStrategy::Sequential) => ConnectionStrategy::Sequential,
        None => ConnectionStrategy::BackupOnly,
    };
    let purpose = if strategy == ConnectionStrategy::BackupOnly {
        ConnectionPurpose::Backup
    } else {
        ConnectionPurpose::Sync
    };
    let mut acknowledgements = Vec::new();
    if strategy == ConnectionStrategy::Sequential {
        acknowledgements.push(SEQUENTIAL_ACKNOWLEDGEMENT.into());
    }
    if strategy == ConnectionStrategy::BackupOnly {
        acknowledgements.push(BACKUP_ACKNOWLEDGEMENT.into());
    }
    if imported.metadata.config.provider == "github_releases" {
        acknowledgements.push(GITHUB_ACKNOWLEDGEMENT.into());
    }
    let prepare = PrepareConnectionRequest {
        config: imported.metadata.config.clone(),
        mode: ConnectionOpenMode::Existing,
        purpose,
        publication_strategy: strategy,
        scope: imported.metadata.descriptor.scope.clone(),
        acknowledgements,
    };
    insert_preparation(&state, prepare, Some(imported))
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
    use std::collections::BTreeMap;

    #[test]
    fn existing_prepare_never_claims_a_key_without_authenticated_recovery() {
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
            publication_strategy: ConnectionStrategy::BackupOnly,
            scope: risunest_external_storage_format::format::Scope {
                library: true,
                referenced_assets: true,
                device_settings: true,
                device_plugins: false,
            },
            acknowledgements: vec![BACKUP_ACKNOWLEDGEMENT.into()],
        };
        let result = insert_preparation(&state, request, None).unwrap();
        assert!(result.requires_recovery_key);
    }

    #[test]
    fn recovery_payload_is_authenticated_before_endpoint_review() {
        let descriptor = Descriptor::new(
            "synthetic-repository".into(),
            risunest_external_storage_format::format::Scope {
                library: true,
                referenced_assets: true,
                device_settings: true,
                device_plugins: false,
            },
            None,
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
            capabilities: super::super::fake::capabilities(false),
            created_at_ms: 1,
        };
        let exported = recovery::export(&stored, &[7; 32]).unwrap();
        let recovered = recovery::import(&exported.bytes, &exported.code).unwrap();
        assert_eq!(
            recovered.metadata.config.endpoint,
            "https://synthetic.invalid"
        );
        let state = ConnectionCommandState::default();
        let material = create_recovery_material_with_state(&state, &stored, &[7; 32]).unwrap();
        let qr = material.qr_payload.expect("synthetic metadata fits a QR");
        let qr_bytes = URL_SAFE_NO_PAD.decode(qr).unwrap();
        assert!(recovery::import(&qr_bytes, &material.code).is_ok());
        let mut damaged = exported.bytes;
        let last = damaged.len() - 1;
        damaged[last] ^= 1;
        assert!(recovery::import(&damaged, &exported.code).is_err());
    }

    #[test]
    fn recovered_google_client_override_must_keep_the_authenticated_project() {
        let descriptor = Descriptor::new(
            "synthetic-repository".into(),
            risunest_external_storage_format::format::Scope {
                library: true,
                referenced_assets: true,
                device_settings: false,
                device_plugins: false,
            },
            Some(super::super::contract::PublicationStrategy::Sequential),
        )
        .unwrap();
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
        let imported = ImportedRecovery {
            metadata: recovery::RecoveryMetadata {
                config: config.clone(),
                descriptor: descriptor.clone(),
                descriptor_locator: super::super::fake::locator(),
                provider_repository_id: "synthetic-provider-root".into(),
            },
            key: Zeroizing::new([7; 32]),
        };
        let mut pending = PendingPreparation {
            request: PrepareConnectionRequest {
                config,
                mode: ConnectionOpenMode::Existing,
                purpose: ConnectionPurpose::Sync,
                publication_strategy: ConnectionStrategy::Sequential,
                scope: descriptor.scope,
                acknowledgements: vec![SEQUENTIAL_ACKNOWLEDGEMENT.into()],
            },
            expires_at_ms: u64::MAX,
            recovery: Some(imported),
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
