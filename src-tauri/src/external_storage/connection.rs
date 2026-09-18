//! Renderer-safe connection DTOs and local validation. Provider credentials and
//! repository keys never implement serialization or debugging in this module.
use super::{
    auth::SecretBytes,
    capabilities::Capabilities,
    connection_store::StoredConnection,
    contract::{ConnectionConfig, ErrorKind, OpenMode, ProviderError, PublicationStrategy, Result},
    control::BackupPointKind,
    durable_quota::MyboxBudget,
    http::{NativeHttpTransport, SystemClock},
    providers::{self, Dependencies},
    secrets,
};
use risunest_external_storage_format::control::BundleSource;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};
use zeroize::{Zeroize, Zeroizing};

pub(crate) const PREPARATION_LIFETIME_MS: u64 = 10 * 60 * 1000;
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

/// What a backup connection captures by default. A synchronization connection
/// has none: what it exchanges is chosen per device under local data.
/// Changing it applies to work started afterwards and never rewrites a point.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CapturePolicy {
    pub hypa: bool,
    pub local_plugins: bool,
    pub local_settings: bool,
}

impl Default for CapturePolicy {
    fn default() -> Self {
        Self {
            hypa: true,
            local_plugins: true,
            local_settings: true,
        }
    }
}

/// Automatic backup points this device made are removed only once they are past
/// both limits. Manual, conflict and recovery-candidate points are never removed.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RetentionPolicy {
    pub keep_count: u32,
    pub keep_days: u32,
}

impl RetentionPolicy {
    pub const DEFAULT: Self = Self {
        keep_count: 10,
        keep_days: 30,
    };
    /// A point another device still expects to reach may not be removed before
    /// that device has had the chance to see it replaced.
    pub const MIN_KEEP_DAYS: u32 = 7;

    pub fn validate(&self) -> Result<()> {
        if !(1..=1000).contains(&self.keep_count)
            || !(Self::MIN_KEEP_DAYS..=3650).contains(&self.keep_days)
        {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        Ok(())
    }
}

/// One backup point as a cleanup reads it. A point document names its bundles
/// but not who made them, so each bundle arrives with the source its own
/// document declares.
pub(crate) struct RetentionPoint {
    pub point_id: String,
    pub kind: BackupPointKind,
    pub created_at_ms: u64,
    pub bundles: Vec<RetentionBundle>,
}

pub(crate) struct RetentionBundle {
    pub object_id: String,
    pub source: BundleSource,
}

/// What a cleanup removes and what it keeps. `roots` names every bundle a kept
/// point holds, both sides of a conflict included.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RetentionDecision {
    pub remove: Vec<String>,
    pub keep: Vec<String>,
    pub roots: Vec<String>,
}

/// Automatic points are counted newest first for each device that made them,
/// and only a point past both the count and the length is expired. Removal is
/// limited to what this device made: another device's points are counted for
/// its own limit and kept here, and so is a point whose bundle came from a
/// synchronized state rather than from a device.
pub(crate) fn decide_retention(
    points: &[RetentionPoint],
    writer_id: &str,
    policy: RetentionPolicy,
    now_ms: u64,
) -> RetentionDecision {
    let keep_for_ms = u64::from(policy.keep_days) * 24 * 60 * 60 * 1000;
    let mut made: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (index, point) in points.iter().enumerate() {
        let [bundle] = point.bundles.as_slice() else {
            continue;
        };
        let BundleSource::Device { writer_id: maker } = &bundle.source else {
            continue;
        };
        if point.kind == BackupPointKind::Automatic {
            made.entry(maker.as_str()).or_default().push(index);
        }
    }
    let mut expired: BTreeMap<usize, &str> = BTreeMap::new();
    for (maker, mut group) in made {
        group.sort_by(|left, right| {
            points[*right]
                .created_at_ms
                .cmp(&points[*left].created_at_ms)
                .then_with(|| points[*left].point_id.cmp(&points[*right].point_id))
        });
        for index in group.into_iter().skip(policy.keep_count as usize) {
            if now_ms.saturating_sub(points[index].created_at_ms) > keep_for_ms {
                expired.insert(index, maker);
            }
        }
    }
    let mut decision = RetentionDecision::default();
    let mut roots = BTreeSet::new();
    for (index, point) in points.iter().enumerate() {
        if expired.get(&index).is_some_and(|maker| *maker == writer_id) {
            decision.remove.push(point.point_id.clone());
            continue;
        }
        decision.keep.push(point.point_id.clone());
        roots.extend(point.bundles.iter().map(|bundle| bundle.object_id.clone()));
    }
    decision.roots = roots.into_iter().collect();
    decision
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PrepareConnectionRequest {
    pub config: ConnectionConfig,
    pub mode: ConnectionOpenMode,
    pub purpose: ConnectionPurpose,
    /// Backup connections only.
    #[serde(default)]
    pub capture_policy: Option<CapturePolicy>,
    pub acknowledgements: Vec<String>,
}

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
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
    },
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
pub(crate) struct PreparedConnection {
    pub preparation_id: String,
    pub expires_at_ms: String,
    pub endpoint: EndpointConfirmation,
    pub capabilities: Option<Capabilities>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture_policy: Option<CapturePolicy>,
    /// The policy in force, which is the default until the user changes it.
    pub retention_policy: RetentionPolicy,
    pub capabilities: Capabilities,
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
            &["sequential", "backup-only"],
            &["koofr"],
        ),
        provider(
            "s3",
            "S3 compatible",
            false,
            &["cas", "sequential", "backup-only"],
            &["aws", "r2", "b2", "hf", "generic"],
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
        authorization_available: true,
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
    // A synchronization connection carries no capture policy: what it
    // exchanges is chosen per device.
    if request.purpose == ConnectionPurpose::Sync && request.capture_policy.is_some() {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let definition = provider_descriptors()
        .into_iter()
        .find(|provider| provider.id == request.config.provider)
        .ok_or_else(|| ProviderError::new(ErrorKind::Unsupported))?;
    if (request.purpose == ConnectionPurpose::Sync
        && !definition.strategies.iter().any(|strategy| {
            matches!(strategy, ConnectionStrategy::Cas | ConnectionStrategy::Sequential)
        }))
        || definition.oauth != request.config.oauth_profile.is_some()
    {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let unique: BTreeSet<&str> = request
        .acknowledgements
        .iter()
        .map(String::as_str)
        .collect();
    if request.acknowledgements.len() > 16
        || unique.iter().any(|value| *value != GITHUB_ACKNOWLEDGEMENT)
        || (request.config.provider == "github_releases"
            && !unique.contains(GITHUB_ACKNOWLEDGEMENT))
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
                Some("aws" | "r2" | "b2" | "hf" | "generic")
            )
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
            )
                && required(&["rootFolderName"])
                && only(&["rootFolderName", "rootFolderId"])
        }
        "github_releases" => {
            config.profile.is_none()
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
            )
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
        account_hint: ((config.provider == "webdav" || config.oauth_profile.is_some())
            && !config.account_id.is_empty())
        .then(|| config.account_id.clone()),
        repository_hint,
        warnings,
        remote_verified,
    })
}

pub(crate) fn strategy_for_new_connection(
    purpose: ConnectionPurpose,
    capabilities: &Capabilities,
) -> Result<Option<PublicationStrategy>> {
    match purpose {
        ConnectionPurpose::Sync => capabilities.automatic_strategy().map(Some),
        ConnectionPurpose::Backup => {
            require_repository_strategy(capabilities, None)?;
            Ok(None)
        }
    }
}

pub(crate) fn require_repository_strategy(
    capabilities: &Capabilities,
    strategy: Option<PublicationStrategy>,
) -> Result<()> {
    match strategy {
        Some(strategy) => capabilities.require(strategy),
        None if capabilities.immutable_create && capabilities.direct_complete_read => Ok(()),
        None => Err(ProviderError::new(ErrorKind::Unsupported)),
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
        display_name: if (connection.config.provider == "webdav"
            || connection.config.oauth_profile.is_some())
            && !connection.config.account_id.is_empty()
        {
            format!(
                "{} · {}",
                connection.config.provider, connection.config.account_id
            )
        } else {
            connection.config.provider.clone()
        },
        endpoint: endpoint_confirmation(&connection.config, true).unwrap_or(EndpointConfirmation {
            provider_id: connection.config.provider.clone(),
            authority: "invalid endpoint".into(),
            account_hint: None,
            repository_hint: "unavailable".into(),
            warnings: Vec::new(),
            remote_verified: false,
        }),
        capture_policy: connection.capture_policy,
        retention_policy: connection
            .retention_policy
            .unwrap_or(RetentionPolicy::DEFAULT),
        capabilities: connection.capabilities.clone(),
        status: ConnectionStatus::Ready,
        last_verified_at_ms: Some(connection.created_at_ms.to_string()),
        last_sync_at_ms: connection.last_sync_at_ms.map(|value| value.to_string()),
        last_backup_at_ms: connection.last_backup_at_ms.map(|value| value.to_string()),
        last_error: None,
    }
}

pub(crate) fn dependencies(root: &Path) -> Result<Dependencies> {
    std::fs::create_dir_all(root).map_err(|_| ProviderError::new(ErrorKind::Transient))?;
    Ok(Dependencies {
        http: Arc::new(NativeHttpTransport::new()?),
        mybox_budget: Arc::new(MyboxBudget::new(root.join("account-quota.sqlite"))),
        requests: super::http::shared_request_state(),
        clock: Arc::new(SystemClock),
        vault: secrets::provider_vault(root),
    })
}

pub(crate) struct EncodedProviderSecret {
    pub bytes: SecretBytes,
    /// Opaque identity derived from the identity-bearing credential field.
    /// This replaces any renderer-provided account label before persistence.
    pub account_id: Option<String>,
}

pub(crate) fn encode_secret(
    provider: &str,
    input: ProviderSecretInput,
) -> Result<EncodedProviderSecret> {
    let invalid = || ProviderError::new(ErrorKind::ReauthRequired);
    let (bytes, account_id) = match (provider, input) {
        ("webdav", ProviderSecretInput::Webdav { password })
            if !password.is_empty()
                && password.len() <= 1024
                && !password.chars().any(char::is_control) =>
        {
            (password.into_bytes(), None)
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
            let account_id =
                super::quota::credential_principal("s3", access_key_id.as_bytes());
            let encoded = serde_json::to_vec(&S3SecretWire {
                access_key_id: &access_key_id,
                secret_access_key: &secret_access_key,
            })
            .map_err(|_| invalid())?;
            access_key_id.zeroize();
            secret_access_key.zeroize();
            (encoded, Some(account_id))
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
            let account_id = super::quota::credential_principal("mybox", pat.as_bytes());
            let encoded = serde_json::to_vec(&MyboxSecretWire {
                pat: &pat,
                expires_at_ms,
            })
            .map_err(|_| invalid())?;
            pat.zeroize();
            (encoded, Some(account_id))
        }
        ("github_releases", ProviderSecretInput::Github { mut token })
            if valid_printable(&token, 512, false) =>
        {
            let account_id =
                super::quota::credential_principal("github_releases", token.as_bytes());
            let encoded =
                serde_json::to_vec(&TokenSecretWire { token: &token }).map_err(|_| invalid())?;
            token.zeroize();
            (encoded, Some(account_id))
        }
        ("gitlab_packages", ProviderSecretInput::Gitlab { mut token })
            if valid_printable(&token, 512, false) =>
        {
            let account_id =
                super::quota::credential_principal("gitlab_packages", token.as_bytes());
            let encoded =
                serde_json::to_vec(&TokenSecretWire { token: &token }).map_err(|_| invalid())?;
            token.zeroize();
            (encoded, Some(account_id))
        }
        _ => return Err(invalid()),
    };
    Ok(EncodedProviderSecret {
        bytes: SecretBytes(Zeroizing::new(bytes)),
        account_id,
    })
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
            capture_policy: (purpose == ConnectionPurpose::Backup)
                .then(CapturePolicy::default),
            acknowledgements: Vec::new(),
        }
    }

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    const NOW_MS: u64 = 1000 * DAY_MS;

    fn made_by(object_id: &str, writer_id: &str) -> RetentionBundle {
        RetentionBundle {
            object_id: object_id.into(),
            source: BundleSource::Device {
                writer_id: writer_id.into(),
            },
        }
    }

    fn point(
        point_id: &str,
        kind: BackupPointKind,
        days_old: u64,
        bundles: Vec<RetentionBundle>,
    ) -> RetentionPoint {
        RetentionPoint {
            point_id: point_id.into(),
            kind,
            created_at_ms: NOW_MS - days_old * DAY_MS,
            bundles,
        }
    }

    fn automatic(point_id: &str, writer_id: &str, days_old: u64) -> RetentionPoint {
        point(
            point_id,
            BackupPointKind::Automatic,
            days_old,
            vec![made_by(&format!("bundle-{point_id}"), writer_id)],
        )
    }

    /// Counting is per device, so a device that has been away keeps its own
    /// backups whatever this device's policy says.
    #[test]
    fn a_cleanup_removes_only_the_automatic_points_this_device_made() {
        let points = [
            automatic("mine-new", "this-device", 1),
            automatic("mine-old", "this-device", 90),
            automatic("mine-older", "this-device", 120),
            automatic("theirs-new", "other-device", 2),
            automatic("theirs-old", "other-device", 95),
            automatic("theirs-older", "other-device", 130),
        ];
        let decision = decide_retention(
            &points,
            "this-device",
            RetentionPolicy {
                keep_count: 1,
                keep_days: 7,
            },
            NOW_MS,
        );
        assert_eq!(decision.remove, ["mine-old", "mine-older"]);
        assert_eq!(
            decision.keep,
            ["mine-new", "theirs-new", "theirs-old", "theirs-older"]
        );
        assert!(decision.roots.contains(&"bundle-theirs-older".to_string()));
        assert!(!decision.roots.contains(&"bundle-mine-old".to_string()));
    }

    /// A kept point, a conflict and a recovery candidate outlive any policy,
    /// and a conflict keeps both of the sides it preserved.
    #[test]
    fn kept_conflict_and_recovery_points_survive_the_narrowest_policy() {
        let points = [
            point(
                "manual",
                BackupPointKind::Manual,
                400,
                vec![made_by("bundle-manual", "this-device")],
            ),
            point(
                "conflict",
                BackupPointKind::Conflict,
                400,
                vec![
                    made_by("bundle-local", "this-device"),
                    made_by("bundle-remote", "other-device"),
                ],
            ),
            point(
                "recovery",
                BackupPointKind::RecoveryCandidate,
                400,
                vec![made_by("bundle-recovery", "this-device")],
            ),
            automatic("automatic", "this-device", 400),
        ];
        let decision = decide_retention(
            &points,
            "this-device",
            RetentionPolicy {
                keep_count: 1,
                keep_days: RetentionPolicy::MIN_KEEP_DAYS,
            },
            NOW_MS,
        );
        assert_eq!(decision.remove, Vec::<String>::new());
        assert_eq!(decision.keep, ["manual", "conflict", "recovery", "automatic"]);
        assert!(decision.roots.contains(&"bundle-local".to_string()));
        assert!(decision.roots.contains(&"bundle-remote".to_string()));
    }

    /// One limit alone never removes anything, and a bundle a synchronized
    /// state produced belongs to no device.
    #[test]
    fn one_limit_alone_and_a_state_bundle_never_remove_a_point() {
        let points = [
            automatic("first", "this-device", 400),
            automatic("second", "this-device", 300),
            automatic("third", "this-device", 200),
        ];
        let within_count = decide_retention(
            &points,
            "this-device",
            RetentionPolicy {
                keep_count: 3,
                keep_days: 7,
            },
            NOW_MS,
        );
        assert_eq!(within_count.remove, Vec::<String>::new());
        let within_days = decide_retention(
            &points,
            "this-device",
            RetentionPolicy {
                keep_count: 1,
                keep_days: 3650,
            },
            NOW_MS,
        );
        assert_eq!(within_days.remove, Vec::<String>::new());

        let synchronized = [
            automatic("device", "this-device", 400),
            point(
                "state",
                BackupPointKind::Automatic,
                400,
                vec![RetentionBundle {
                    object_id: "bundle-state".into(),
                    source: BundleSource::SyncState {
                        commit_id: "commit".into(),
                    },
                }],
            ),
        ];
        let decision = decide_retention(
            &synchronized,
            "this-device",
            RetentionPolicy {
                keep_count: 1,
                keep_days: 7,
            },
            NOW_MS,
        );
        assert_eq!(decision.remove, Vec::<String>::new());
        assert_eq!(decision.keep, ["device", "state"]);
    }

    #[test]
    fn a_retention_policy_holds_the_shortest_grace_and_a_first_backup() {
        assert!(RetentionPolicy::DEFAULT.validate().is_ok());
        for policy in [
            RetentionPolicy {
                keep_count: 1,
                keep_days: RetentionPolicy::MIN_KEEP_DAYS,
            },
            RetentionPolicy {
                keep_count: 1000,
                keep_days: 3650,
            },
        ] {
            assert!(policy.validate().is_ok());
        }
        for policy in [
            RetentionPolicy {
                keep_count: 0,
                keep_days: 30,
            },
            RetentionPolicy {
                keep_count: 1001,
                keep_days: 30,
            },
            RetentionPolicy {
                keep_count: 10,
                keep_days: RetentionPolicy::MIN_KEEP_DAYS - 1,
            },
            RetentionPolicy {
                keep_count: 10,
                keep_days: 3651,
            },
        ] {
            assert!(matches!(
                policy.validate(),
                Err(ProviderError {
                    kind: ErrorKind::PreconditionFailed,
                    ..
                })
            ));
        }
    }

    #[test]
    fn local_prepare_needs_no_strategy_approval_and_rejects_sync_device_scope() {
        let mut sync = request("webdav", ConnectionPurpose::Sync);
        assert!(validate_preparation(&sync).is_ok());
        sync.acknowledgements.push("sequential-single-device".into());
        assert!(validate_preparation(&sync).is_err());
        sync.acknowledgements.clear();
        sync.capture_policy = Some(CapturePolicy::default());
        assert!(validate_preparation(&sync).is_err());
    }

    #[test]
    fn renderer_cannot_choose_a_publication_strategy() {
        let config = request("webdav", ConnectionPurpose::Sync).config;
        let mut encoded = serde_json::json!({
            "config": config, "mode": "create", "purpose": "sync", "acknowledgements": []
        });
        assert!(serde_json::from_value::<PrepareConnectionRequest>(encoded.clone()).is_ok());
        encoded["publicationStrategy"] = serde_json::json!("sequential");
        assert!(serde_json::from_value::<PrepareConnectionRequest>(encoded).is_err());
    }

    #[test]
    fn a_backup_only_provider_never_silently_changes_a_sync_purpose() {
        let sync = request("gitlab_packages", ConnectionPurpose::Sync);
        assert!(matches!(validate_preparation(&sync), Err(ProviderError {
            kind: ErrorKind::Unsupported, ..
        })));
        assert!(matches!(sync.purpose, ConnectionPurpose::Sync));
    }

    #[test]
    fn new_sync_strategy_is_automatic_but_an_existing_descriptor_never_downgrades() {
        let mut capabilities = Capabilities {
            immutable_create: true,
            direct_complete_read: true,
            atomic_create_head: true,
            conditional_head_update: true,
            stable_head_replace: true,
            head_read_after_write: true,
            head_retry_control: true,
            ..Default::default()
        };
        assert_eq!(strategy_for_new_connection(ConnectionPurpose::Sync, &capabilities).unwrap(),
            Some(PublicationStrategy::Cas));
        assert_eq!(strategy_for_new_connection(ConnectionPurpose::Backup, &capabilities).unwrap(), None);
        capabilities.conditional_head_update = false;
        assert_eq!(strategy_for_new_connection(ConnectionPurpose::Sync, &capabilities).unwrap(),
            Some(PublicationStrategy::Sequential));
        assert_eq!(require_repository_strategy(&capabilities, Some(PublicationStrategy::Cas))
            .unwrap_err().kind, ErrorKind::Unsupported);
        capabilities.stable_head_replace = false;
        assert_eq!(strategy_for_new_connection(ConnectionPurpose::Sync, &capabilities)
            .unwrap_err().kind, ErrorKind::Unsupported);
        assert_eq!(strategy_for_new_connection(ConnectionPurpose::Backup, &capabilities).unwrap(), None);
    }

    #[test]
    fn local_prepare_rejects_an_incomplete_provider_location() {
        let mut request = request("webdav", ConnectionPurpose::Backup);
        request.config.location.clear();
        assert!(validate_preparation(&request).is_err());
    }

    #[test]
    fn non_webdav_preparation_needs_no_renderer_account_label_and_accepts_aws() {
        let mut s3 = request("s3", ConnectionPurpose::Backup);
        s3.config.profile = Some("aws".into());
        s3.config.account_id.clear();
        s3.config.location = BTreeMap::from([
            ("bucket".into(), "synthetic-bucket".into()),
            ("region".into(), "us-east-1".into()),
        ]);
        assert!(validate_preparation(&s3).is_ok());

        let mut mybox = request("mybox", ConnectionPurpose::Backup);
        mybox.config.account_id.clear();
        mybox.config.location =
            BTreeMap::from([("rootFolderName".into(), "RisuNest".into())]);
        assert!(validate_preparation(&mybox).is_ok());

        let mut github = request("github_releases", ConnectionPurpose::Backup);
        github.config.account_id.clear();
        github.config.location = BTreeMap::from([
            ("uploadEndpoint".into(), "https://uploads.github.com".into()),
            ("owner".into(), "synthetic-owner".into()),
            ("repo".into(), "synthetic-repository".into()),
            ("tagPrefix".into(), "risunest".into()),
        ]);
        github
            .acknowledgements
            .push(GITHUB_ACKNOWLEDGEMENT.into());
        assert!(validate_preparation(&github).is_ok());
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
        assert!(google.authorization_available);
        assert!(webdav.authorization_available);
    }

    #[test]
    fn credential_fingerprints_never_reach_connection_titles() {
        let account_id = super::super::quota::credential_principal(
            "mybox",
            b"synthetic-pat",
        );
        let connection = StoredConnection {
            id: "synthetic-connection".into(),
            config: ConnectionConfig {
                provider: "mybox".into(),
                profile: Some("plan30gb".into()),
                endpoint: "https://open-api.mybox.naver.com/v1".into(),
                account_id: account_id.clone(),
                location: BTreeMap::from([(
                    "rootFolderName".into(),
                    "RisuNest".into(),
                )]),
                oauth_profile: None,
            },
            descriptor: risunest_external_storage_format::format::Descriptor::new(
                "synthetic-repository".into(),
                None,
            )
            .unwrap(),
            descriptor_locator: super::super::fake::locator(),
            provider_repository_id: "synthetic-root".into(),
            credential_ref: "synthetic-credential-ref".into(),
            root_key_ref: "synthetic-key-ref".into(),
            capabilities: Capabilities::default(),
            created_at_ms: 1,
            last_sync_at_ms: None,
            last_backup_at_ms: None,
            capture_policy: Some(CapturePolicy::default()),
            retention_policy: None,
        };
        let summary = summary(&connection);
        assert_eq!(summary.display_name, "mybox");
        assert_eq!(summary.endpoint.account_hint, None);
        let serialized = serde_json::to_string(&summary).unwrap();
        assert!(!serialized.contains(&account_id));
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
        let decoded: serde_json::Value = serde_json::from_slice(&encoded.bytes.0).unwrap();
        assert_eq!(decoded["accessKeyId"], "synthetic-id");
        assert_eq!(decoded["secretAccessKey"], "synthetic-key");
        assert_eq!(
            encoded.account_id,
            Some(super::super::quota::credential_principal(
                "s3",
                b"synthetic-id"
            ))
        );

        let first = encode_secret(
            "mybox",
            ProviderSecretInput::Mybox {
                pat: "synthetic-pat".into(),
                expires_at_ms: u64::MAX.to_string(),
            },
        )
        .unwrap();
        let renewed = encode_secret(
            "mybox",
            ProviderSecretInput::Mybox {
                pat: "synthetic-pat".into(),
                expires_at_ms: (u64::MAX - 1).to_string(),
            },
        )
        .unwrap();
        let other = encode_secret(
            "mybox",
            ProviderSecretInput::Mybox {
                pat: "different-pat".into(),
                expires_at_ms: u64::MAX.to_string(),
            },
        )
        .unwrap();
        assert_eq!(first.account_id, renewed.account_id);
        assert_ne!(first.account_id, other.account_id);
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
            "token": "synthetic-token"
        }))
        .unwrap();
        assert!(matches!(parsed, ProviderSecretInput::Gitlab { .. }));
        assert!(serde_json::from_value::<ProviderSecretInput>(serde_json::json!({
            "kind": "gitlab",
            "token": "synthetic-token",
            "tokenKind": "projectAccessToken"
        }))
        .is_err());
    }
}
