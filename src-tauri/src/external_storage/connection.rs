//! Renderer-safe connection DTOs and local validation. Provider credentials and
//! repository keys never implement serialization or debugging in this module.
use super::{
    auth::SecretBytes,
    capabilities::{Capabilities, Evidence},
    connection_store::StoredConnection,
    contract::{ConnectionConfig, ErrorKind, OpenMode, ProviderError, PublicationStrategy, Result},
    durable_quota::DurableBudget,
    http::{NativeHttpTransport, SystemClock},
    providers::{self, Dependencies},
    secrets,
};
use risunest_external_storage_format::format::Scope;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeSet, path::Path, sync::Arc};
use zeroize::{Zeroize, Zeroizing};

pub(crate) const PREPARATION_LIFETIME_MS: u64 = 10 * 60 * 1000;
pub(crate) const SEQUENTIAL_ACKNOWLEDGEMENT: &str = "sequential-single-device";
pub(crate) const GITHUB_ACKNOWLEDGEMENT: &str = "github-dedicated-private-repository";

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ConnectionPurpose {
    Backup,
    Sync,
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ConnectionStrategy {
    Cas,
    Sequential,
    BackupOnly,
}

impl ConnectionStrategy {
    pub(crate) fn descriptor(self) -> Option<PublicationStrategy> {
        match self {
            Self::Cas => Some(PublicationStrategy::Cas),
            Self::Sequential => Some(PublicationStrategy::Sequential),
            Self::BackupOnly => None,
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ConnectionOpenMode {
    Create,
    Existing,
}

impl From<ConnectionOpenMode> for OpenMode {
    fn from(value: ConnectionOpenMode) -> Self {
        match value {
            ConnectionOpenMode::Create => Self::Create,
            ConnectionOpenMode::Existing => Self::Existing,
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrepareConnectionRequest {
    pub config: ConnectionConfig,
    pub mode: ConnectionOpenMode,
    pub purpose: ConnectionPurpose,
    pub publication_strategy: ConnectionStrategy,
    pub scope: Scope,
    pub acknowledgements: Vec<String>,
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum ProviderSecretInput {
    Webdav {
        password: String,
    },
    S3 {
        access_key_id: String,
        secret_access_key: String,
    },
    Mybox {
        pat: String,
        expires_at_ms: String,
    },
    Github {
        token: String,
    },
    Gitlab {
        token: String,
        token_kind: GitlabTokenKind,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum GitlabTokenKind {
    DeployToken,
    PersonalAccessToken,
    ProjectAccessToken,
}

impl GitlabTokenKind {
    fn as_str(&self) -> &'static str {
        match self {
            Self::DeployToken => "deployToken",
            Self::PersonalAccessToken => "personalAccessToken",
            Self::ProjectAccessToken => "projectAccessToken",
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct S3SecretWire<'a> {
    access_key_id: &'a str,
    secret_access_key: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MyboxSecretWire<'a> {
    pat: &'a str,
    expires_at_ms: u64,
}

#[derive(Serialize)]
struct TokenSecretWire<'a> {
    token: &'a str,
}

#[derive(Serialize)]
struct GitlabSecretWire<'a> {
    token: &'a str,
    kind: &'static str,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EndpointConfirmation {
    pub provider_id: String,
    pub authority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_hint: Option<String>,
    pub repository_hint: String,
    pub warnings: Vec<String>,
    pub remote_verified: bool,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectionCapabilities {
    pub cas: bool,
    pub sequential: bool,
    pub backup_only: bool,
    pub resumable_upload: bool,
    pub range_download: bool,
    pub snapshot_discovery: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_stored_bytes: Option<String>,
    pub evidence: CapabilityEvidence,
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum CapabilityEvidence {
    Live,
    Synthetic,
    Unverified,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreparedConnection {
    pub preparation_id: String,
    pub expires_at_ms: String,
    pub endpoint: EndpointConfirmation,
    pub capabilities: ConnectionCapabilities,
    pub requires_o_auth: bool,
    pub requires_recovery_key: bool,
    pub requires_platform_o_auth_client: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth_project_hint: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProviderDescriptor {
    pub id: String,
    pub display_name: String,
    pub oauth: bool,
    pub authorization_available: bool,
    pub strategies: Vec<ConnectionStrategy>,
    pub profiles: Vec<String>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectionErrorSummary {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub action: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ConnectionStatus {
    Ready,
    Paused,
    ReauthRequired,
    KeyLocked,
    Error,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectionSummary {
    pub id: String,
    pub provider_id: String,
    pub purpose: ConnectionPurpose,
    pub strategy: ConnectionStrategy,
    pub mode: ConnectionOpenMode,
    pub display_name: String,
    pub endpoint: EndpointConfirmation,
    pub scope: Scope,
    pub capabilities: ConnectionCapabilities,
    pub status: ConnectionStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_verified_at_ms: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_sync_at_ms: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_backup_at_ms: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<ConnectionErrorSummary>,
}

pub(crate) fn provider_descriptors() -> Vec<ProviderDescriptor> {
    vec![
        provider(
            "webdav",
            "WebDAV / Koofr",
            false,
            &["cas", "sequential", "backup-only"],
            &["koofr"],
        ),
        provider(
            "s3",
            "S3 compatible",
            false,
            &["cas", "sequential", "backup-only"],
            &["r2", "b2", "hf", "generic"],
        ),
        provider(
            "google_drive",
            "Google Drive",
            true,
            &["sequential", "backup-only"],
            &["drive"],
        ),
        // No CAS: Graph has no conditional update on the content PUT used for a head.
        provider(
            "onedrive",
            "OneDrive",
            true,
            &["sequential", "backup-only"],
            &[],
        ),
        provider(
            "mybox",
            "NAVER MYBOX",
            false,
            &["sequential", "backup-only"],
            &[
                "plan30gb",
                "plan80gb",
                "plan180gb",
                "plan2tb",
                "plan5tb",
                "plan10tb",
                "plan20tb",
            ],
        ),
        provider(
            "github_releases",
            "GitHub Releases",
            false,
            &["backup-only"],
            &[],
        ),
        provider(
            "gitlab_packages",
            "GitLab Generic Packages",
            false,
            &["backup-only"],
            &["gitlabCom", "selfManaged"],
        ),
    ]
}

fn provider(
    id: &str,
    display_name: &str,
    oauth: bool,
    strategies: &[&str],
    profiles: &[&str],
) -> ProviderDescriptor {
    ProviderDescriptor {
        id: id.into(),
        display_name: display_name.into(),
        oauth,
        authorization_available: !oauth || !cfg!(target_os = "ios"),
        strategies: strategies
            .iter()
            .map(|value| match *value {
                "cas" => ConnectionStrategy::Cas,
                "sequential" => ConnectionStrategy::Sequential,
                _ => ConnectionStrategy::BackupOnly,
            })
            .collect(),
        profiles: profiles.iter().map(|value| (*value).into()).collect(),
    }
}

pub(crate) fn validate_preparation(
    request: &PrepareConnectionRequest,
) -> Result<EndpointConfirmation> {
    #[cfg(target_os = "ios")]
    if request.config.oauth_profile.is_some() {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    if !request.scope.library || !request.scope.referenced_assets {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    if request.purpose == ConnectionPurpose::Sync
        && (request.publication_strategy == ConnectionStrategy::BackupOnly
            || request.scope.device_settings
            || request.scope.device_plugins)
    {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    if request.purpose == ConnectionPurpose::Backup
        && request.publication_strategy != ConnectionStrategy::BackupOnly
    {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let definition = provider_descriptors()
        .into_iter()
        .find(|provider| provider.id == request.config.provider)
        .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
    if !definition
        .strategies
        .contains(&request.publication_strategy)
        || definition.oauth != request.config.oauth_profile.is_some()
    {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let unique: BTreeSet<&str> = request
        .acknowledgements
        .iter()
        .map(String::as_str)
        .collect();
    let required = [
        (
            request.publication_strategy == ConnectionStrategy::Sequential,
            SEQUENTIAL_ACKNOWLEDGEMENT,
        ),
        (
            request.config.provider == "github_releases",
            GITHUB_ACKNOWLEDGEMENT,
        ),
    ];
    if request.acknowledgements.len() > 16
        || required
            .iter()
            .any(|(needed, value)| *needed && !unique.contains(value))
    {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    validate_config_shape(&request.config)?;
    endpoint_confirmation(&request.config, false)
}

pub(crate) fn validate_config_shape(config: &ConnectionConfig) -> Result<()> {
    let invalid = || ProviderError::new(ErrorKind::Unsupported);
    if config.provider.len() > 64
        || config.endpoint.len() > 4096
        || config.account_id.len() > 512
        || config.location.len() > 16
        || config.location.iter().any(|(key, value)| {
            key.is_empty()
                || key.len() > 64
                || value.len() > 4096
                || key.contains('\0')
                || value.contains('\0')
        })
    {
        return Err(invalid());
    }
    let endpoint = if config.endpoint.is_empty() && config.provider == "google_drive" {
        "https://www.googleapis.com"
    } else {
        config.endpoint.as_str()
    };
    let url = url::Url::parse(endpoint).map_err(|_| invalid())?;
    if url.scheme() != "https"
        || url.host_str().is_none_or(str::is_empty)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(invalid());
    }
    if let Some(profile) = &config.oauth_profile {
        if profile.project_id.is_empty()
            || profile.project_id.len() > 512
            || profile.platform_client_ids.is_empty()
            || profile.platform_client_ids.len() > 8
            || profile
                .platform_client_ids
                .iter()
                .any(|(platform, client)| {
                    platform.is_empty()
                        || platform.len() > 32
                        || client.is_empty()
                        || client.len() > 1024
                })
        {
            return Err(invalid());
        }
    }
    let required = |keys: &[&str]| {
        keys.iter().all(|key| {
            config
                .location
                .get(*key)
                .is_some_and(|value| !value.trim().is_empty())
        })
    };
    let only = |keys: &[&str]| {
        config
            .location
            .keys()
            .all(|key| keys.contains(&key.as_str()))
    };
    let account = || {
        !config.account_id.trim().is_empty()
            && config.account_id.len() <= 256
            && !config.account_id.chars().any(char::is_control)
    };
    let valid = match config.provider.as_str() {
        "webdav" => {
            matches!(config.profile.as_deref(), None | Some("koofr"))
                && account()
                && !config.account_id.contains(':')
                && required(&["root"])
                && only(&["root"])
        }
        "s3" => {
            matches!(
                config.profile.as_deref(),
                Some("r2" | "b2" | "hf" | "generic")
            ) && account()
                && required(&["bucket"])
                && only(&["bucket", "prefix", "region", "addressing"])
                && config
                    .location
                    .get("addressing")
                    .is_none_or(|value| matches!(value.as_str(), "path" | "virtual"))
        }
        "google_drive" => {
            matches!(config.profile.as_deref(), None | Some("drive"))
                && required(&["folderId"])
                && only(&["folderId", "space"])
                && config
                    .location
                    .get("space")
                    .is_none_or(|value| matches!(value.as_str(), "drive" | "appDataFolder"))
        }
        "onedrive" => {
            config.profile.is_none()
                && required(&[
                    "accountType",
                    "tenant",
                    "driveId",
                    "rootItemId",
                    "redirectUri",
                ])
                && only(&[
                    "accountType",
                    "tenant",
                    "driveId",
                    "rootItemId",
                    "redirectUri",
                ])
                && config.location.get("accountType").is_some_and(|value| {
                    matches!(value.as_str(), "personal" | "business" | "appFolder")
                })
        }
        "mybox" => {
            matches!(
                config.profile.as_deref(),
                None | Some(
                    "plan30gb"
                        | "plan80gb"
                        | "plan180gb"
                        | "plan2tb"
                        | "plan5tb"
                        | "plan10tb"
                        | "plan20tb"
                )
            ) && account()
                && required(&["rootFolderName"])
                && only(&["rootFolderName", "rootFolderId"])
        }
        "github_releases" => {
            config.profile.is_none()
                && account()
                && required(&["uploadEndpoint", "owner", "repo", "tagPrefix"])
                && only(&["uploadEndpoint", "owner", "repo", "tagPrefix"])
                && config
                    .location
                    .get("uploadEndpoint")
                    .and_then(|value| url::Url::parse(value).ok())
                    .is_some_and(|url| url.scheme() == "https" && url.host_str().is_some())
        }
        "gitlab_packages" => {
            matches!(
                config.profile.as_deref(),
                None | Some("gitlabCom" | "selfManaged")
            ) && account()
                && required(&["projectId", "packageName"])
                && only(&["projectId", "packageName", "maxFileBytes"])
                && config
                    .location
                    .get("maxFileBytes")
                    .is_none_or(|value| value.parse::<u64>().is_ok_and(|value| value > 0))
        }
        _ => false,
    };
    if !valid {
        return Err(invalid());
    }
    Ok(())
}

pub(crate) fn endpoint_confirmation(
    config: &ConnectionConfig,
    remote_verified: bool,
) -> Result<EndpointConfirmation> {
    let endpoint = if config.endpoint.is_empty() && config.provider == "google_drive" {
        "https://www.googleapis.com"
    } else {
        config.endpoint.as_str()
    };
    let url = url::Url::parse(endpoint).map_err(|_| ProviderError::new(ErrorKind::Unsupported))?;
    let host = url
        .host_str()
        .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
    let authority = match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    };
    let repository_hint = match config.provider.as_str() {
        "webdav" => config.location.get("root").cloned(),
        "s3" => config.location.get("bucket").map(|bucket| {
            format!(
                "{bucket}/{}",
                config
                    .location
                    .get("prefix")
                    .map(String::as_str)
                    .unwrap_or_default()
            )
        }),
        "google_drive" => config.location.get("folderId").cloned(),
        "onedrive" => config.location.get("rootItemId").cloned(),
        "mybox" => config.location.get("rootFolderName").cloned(),
        "github_releases" => Some(format!(
            "{}/{}",
            config
                .location
                .get("owner")
                .map(String::as_str)
                .unwrap_or("?"),
            config
                .location
                .get("repo")
                .map(String::as_str)
                .unwrap_or("?")
        )),
        "gitlab_packages" => config.location.get("projectId").cloned(),
        _ => None,
    }
    .unwrap_or_else(|| "configured repository".into());
    // Warning codes; the UI owns the localized wording.
    let warnings = match config.provider.as_str() {
        "github_releases" => vec!["github-dedicated-repository".into()],
        "gitlab_packages" => vec!["gitlab-cleanup-policy".into()],
        _ => Vec::new(),
    };
    Ok(EndpointConfirmation {
        provider_id: config.provider.clone(),
        authority,
        account_hint: (!config.account_id.is_empty()).then(|| config.account_id.clone()),
        repository_hint,
        warnings,
        remote_verified,
    })
}

pub(crate) fn predicted_capabilities(strategy: ConnectionStrategy) -> ConnectionCapabilities {
    ConnectionCapabilities {
        cas: strategy == ConnectionStrategy::Cas,
        sequential: strategy == ConnectionStrategy::Sequential,
        backup_only: strategy == ConnectionStrategy::BackupOnly,
        resumable_upload: false,
        range_download: false,
        snapshot_discovery: false,
        max_stored_bytes: None,
        evidence: CapabilityEvidence::Unverified,
    }
}

pub(crate) fn capabilities(value: &Capabilities) -> ConnectionCapabilities {
    let evidence = [
        value.immutable_create,
        value.direct_complete_read,
        value.atomic_create_head,
        value.conditional_head_update,
        value.stable_head_replace,
        value.head_read_after_write,
        value.head_retry_control,
        value.snapshot_discovery,
    ];
    let evidence = if evidence.iter().all(|value| *value == Evidence::Live) {
        CapabilityEvidence::Live
    } else if evidence.iter().any(|value| *value == Evidence::Synthetic) {
        CapabilityEvidence::Synthetic
    } else {
        CapabilityEvidence::Unverified
    };
    ConnectionCapabilities {
        cas: value.require(PublicationStrategy::Cas).is_ok(),
        sequential: value.require(PublicationStrategy::Sequential).is_ok(),
        backup_only: value.immutable_create != Evidence::Unverified
            && value.direct_complete_read != Evidence::Unverified,
        resumable_upload: value.resumable_upload,
        range_download: value.range,
        snapshot_discovery: value.snapshot_discovery != Evidence::Unverified,
        max_stored_bytes: value.max_stored_bytes.map(|value| value.to_string()),
        evidence,
    }
}

pub(crate) fn summary(connection: &StoredConnection) -> ConnectionSummary {
    let strategy = match connection.descriptor.publication_strategy {
        Some(PublicationStrategy::Cas) => ConnectionStrategy::Cas,
        Some(PublicationStrategy::Sequential) => ConnectionStrategy::Sequential,
        None => ConnectionStrategy::BackupOnly,
    };
    ConnectionSummary {
        id: connection.id.clone(),
        provider_id: connection.config.provider.clone(),
        purpose: if strategy == ConnectionStrategy::BackupOnly {
            ConnectionPurpose::Backup
        } else {
            ConnectionPurpose::Sync
        },
        strategy,
        mode: ConnectionOpenMode::Existing,
        display_name: if connection.config.account_id.is_empty() {
            connection.config.provider.clone()
        } else {
            format!(
                "{} · {}",
                connection.config.provider, connection.config.account_id
            )
        },
        endpoint: endpoint_confirmation(&connection.config, true).unwrap_or(EndpointConfirmation {
            provider_id: connection.config.provider.clone(),
            authority: "invalid endpoint".into(),
            account_hint: None,
            repository_hint: "unavailable".into(),
            warnings: Vec::new(),
            remote_verified: false,
        }),
        scope: connection.descriptor.scope.clone(),
        capabilities: capabilities(&connection.capabilities),
        status: ConnectionStatus::Ready,
        last_verified_at_ms: Some(connection.created_at_ms.to_string()),
        last_sync_at_ms: None,
        last_backup_at_ms: None,
        last_error: None,
    }
}

pub(crate) fn dependencies(root: &Path) -> Result<(Dependencies, Arc<DurableBudget>)> {
    std::fs::create_dir_all(root).map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    let budget = Arc::new(DurableBudget::open(&root.join("account-quota.sqlite"))?);
    let dependencies = Dependencies {
        http: Arc::new(NativeHttpTransport::new()?),
        budget: budget.clone(),
        clock: Arc::new(SystemClock),
        vault: secrets::provider_vault(root),
    };
    Ok((dependencies, budget))
}

pub(crate) fn encode_secret(provider: &str, input: ProviderSecretInput) -> Result<SecretBytes> {
    let invalid = || ProviderError::new(ErrorKind::ReauthRequired);
    let bytes = match (provider, input) {
        ("webdav", ProviderSecretInput::Webdav { password })
            if !password.is_empty()
                && password.len() <= 1024
                && !password.chars().any(char::is_control) =>
        {
            password.into_bytes()
        }
        (
            "s3",
            ProviderSecretInput::S3 {
                mut access_key_id,
                mut secret_access_key,
            },
        ) if valid_printable(&access_key_id, 256, true)
            && valid_printable(&secret_access_key, 1024, true) =>
        {
            let encoded = serde_json::to_vec(&S3SecretWire {
                access_key_id: &access_key_id,
                secret_access_key: &secret_access_key,
            })
            .map_err(|_| invalid())?;
            access_key_id.zeroize();
            secret_access_key.zeroize();
            encoded
        }
        (
            "mybox",
            ProviderSecretInput::Mybox {
                mut pat,
                expires_at_ms,
            },
        ) if valid_printable(&pat, 4096, false) => {
            let now_ms: u64 = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            let expires_at_ms = expires_at_ms
                .parse::<u64>()
                .ok()
                .filter(|value| *value > now_ms)
                .ok_or_else(invalid)?;
            let encoded = serde_json::to_vec(&MyboxSecretWire {
                pat: &pat,
                expires_at_ms,
            })
            .map_err(|_| invalid())?;
            pat.zeroize();
            encoded
        }
        ("github_releases", ProviderSecretInput::Github { mut token })
            if valid_printable(&token, 512, false) =>
        {
            let encoded =
                serde_json::to_vec(&TokenSecretWire { token: &token }).map_err(|_| invalid())?;
            token.zeroize();
            encoded
        }
        (
            "gitlab_packages",
            ProviderSecretInput::Gitlab {
                mut token,
                token_kind,
            },
        ) if valid_printable(&token, 512, false) => {
            let encoded = serde_json::to_vec(&GitlabSecretWire {
                token: &token,
                kind: token_kind.as_str(),
            })
            .map_err(|_| invalid())?;
            token.zeroize();
            encoded
        }
        _ => return Err(invalid()),
    };
    Ok(SecretBytes(Zeroizing::new(bytes)))
}

fn valid_printable(value: &str, max: usize, allow_space: bool) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || (allow_space && byte == b' '))
        && (!allow_space || value.trim() == value)
}

pub(crate) fn provider_for(
    config: &ConnectionConfig,
    dependencies: Dependencies,
) -> Result<Arc<dyn super::contract::Provider>> {
    providers::create(&config.provider, dependencies)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn request(
        provider: &str,
        purpose: ConnectionPurpose,
        strategy: ConnectionStrategy,
    ) -> PrepareConnectionRequest {
        PrepareConnectionRequest {
            config: ConnectionConfig {
                provider: provider.into(),
                profile: None,
                endpoint: "https://synthetic.invalid/root".into(),
                account_id: "synthetic-account".into(),
                location: BTreeMap::from([("root".into(), "RisuNest".into())]),
                oauth_profile: None,
            },
            mode: ConnectionOpenMode::Create,
            purpose,
            publication_strategy: strategy,
            scope: Scope {
                library: true,
                referenced_assets: true,
                device_settings: purpose == ConnectionPurpose::Backup,
                device_plugins: false,
            },
            acknowledgements: match strategy {
                ConnectionStrategy::Sequential => vec![SEQUENTIAL_ACKNOWLEDGEMENT.into()],
                ConnectionStrategy::BackupOnly => Vec::new(),
                ConnectionStrategy::Cas => Vec::new(),
            },
        }
    }

    #[test]
    fn local_prepare_rejects_missing_acknowledgement_and_sync_device_scope() {
        let mut sequential = request(
            "webdav",
            ConnectionPurpose::Sync,
            ConnectionStrategy::Sequential,
        );
        sequential.acknowledgements.clear();
        assert!(validate_preparation(&sequential).is_err());
        sequential
            .acknowledgements
            .push(SEQUENTIAL_ACKNOWLEDGEMENT.into());
        sequential.scope.device_settings = true;
        assert!(validate_preparation(&sequential).is_err());
    }

    #[test]
    fn local_prepare_rejects_an_incomplete_provider_location() {
        let mut request = request(
            "webdav",
            ConnectionPurpose::Backup,
            ConnectionStrategy::BackupOnly,
        );
        request.config.location.clear();
        assert!(validate_preparation(&request).is_err());
    }

    #[test]
    fn provider_registry_reports_current_platform_oauth_availability() {
        let providers = provider_descriptors();
        let google = providers
            .iter()
            .find(|provider| provider.id == "google_drive")
            .unwrap();
        let webdav = providers
            .iter()
            .find(|provider| provider.id == "webdav")
            .unwrap();
        assert_eq!(google.authorization_available, !cfg!(target_os = "ios"));
        assert!(webdav.authorization_available);
    }

    #[test]
    fn secret_encoding_matches_adapter_owned_payload_shapes_without_debugging_values() {
        let encoded = encode_secret(
            "s3",
            ProviderSecretInput::S3 {
                access_key_id: "synthetic-id".into(),
                secret_access_key: "synthetic-key".into(),
            },
        )
        .unwrap();
        let decoded: serde_json::Value = serde_json::from_slice(&encoded.0).unwrap();
        assert_eq!(decoded["accessKeyId"], "synthetic-id");
        assert_eq!(decoded["secretAccessKey"], "synthetic-key");
        assert!(encode_secret(
            "google_drive",
            ProviderSecretInput::Github {
                token: "synthetic".into()
            }
        )
        .is_err());
    }

    #[test]
    fn renderer_secret_union_accepts_camel_case_fields() {
        let parsed: ProviderSecretInput = serde_json::from_value(serde_json::json!({
            "kind": "s3",
            "accessKeyId": "synthetic-access",
            "secretAccessKey": "synthetic-secret"
        }))
        .unwrap();
        assert!(matches!(parsed, ProviderSecretInput::S3 { .. }));

        let parsed: ProviderSecretInput = serde_json::from_value(serde_json::json!({
            "kind": "gitlab",
            "token": "synthetic-token",
            "tokenKind": "projectAccessToken"
        }))
        .unwrap();
        assert!(matches!(
            parsed,
            ProviderSecretInput::Gitlab {
                token_kind: GitlabTokenKind::ProjectAccessToken,
                ..
            }
        ));
    }
}
