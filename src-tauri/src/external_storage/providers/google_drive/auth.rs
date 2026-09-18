//! Secret payload, refresh-token rotation and the installed-application
//! authorization policy. Token material never leaves this module except as an
//! `Authorization` header value or a sealed vault payload.
use super::{
    config::{self, AuthorizationSettings, Settings},
    wire::{self, About},
};
use crate::external_storage::{
    auth::{AuthorizationCode, AuthorizationPolicy, SecretBytes},
    contract::*,
    http::HttpRequest,
    providers::Dependencies,
    quota::AccountKey,
};
use serde::Deserialize;
use std::{collections::BTreeMap, sync::Mutex};
use zeroize::Zeroizing;

/// A token is refreshed this far before its stated expiry.
const EXPIRY_SKEW_MS: u64 = 60 * 1000;
const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

pub(super) struct CachedToken {
    pub access: Zeroizing<String>,
    pub expires_at_ms: u64,
}
impl CachedToken {
    fn duplicate(&self) -> Self {
        Self {
            access: Zeroizing::new(self.access.to_string()),
            expires_at_ms: self.expires_at_ms,
        }
    }
}

// No Debug, Serialize or Display: the payload is written back through the
// vault only, by `encode` below.
struct SecretPayload {
    refresh_token: Zeroizing<String>,
    access_token: Option<Zeroizing<String>>,
    expires_at_ms: Option<u64>,
    client_secret: Option<Zeroizing<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredSecret {
    refresh_token: String,
    access_token: Option<String>,
    access_token_expires_at_ms: Option<u64>,
    client_secret: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct TokenInfo {
    issued_to: Option<String>,
    scope: Option<String>,
}

pub(crate) struct AuthorizedSecret {
    pub secret: SecretBytes,
    pub account_id: String,
}
fn reauth() -> ProviderError {
    ProviderError::new(ErrorKind::ReauthRequired)
}

fn decode(bytes: &SecretBytes) -> Result<SecretPayload> {
    let stored: StoredSecret = serde_json::from_slice(&bytes.0).map_err(|_| reauth())?;
    if stored.refresh_token.is_empty() {
        return Err(reauth());
    }
    Ok(SecretPayload {
        refresh_token: Zeroizing::new(stored.refresh_token),
        access_token: stored.access_token.map(Zeroizing::new),
        expires_at_ms: stored.access_token_expires_at_ms,
        client_secret: stored.client_secret.map(Zeroizing::new),
    })
}

fn encode(payload: &SecretPayload) -> Result<SecretBytes> {
    let corrupt = || ProviderError::new(ErrorKind::Corrupt);
    let mut text = Zeroizing::new(String::from("{\"refreshToken\":"));
    text.push_str(&serde_json::to_string(payload.refresh_token.as_str()).map_err(|_| corrupt())?);
    if let Some(access) = &payload.access_token {
        text.push_str(",\"accessToken\":");
        text.push_str(&serde_json::to_string(access.as_str()).map_err(|_| corrupt())?);
    }
    if let Some(expires) = payload.expires_at_ms {
        text.push_str(&format!(",\"accessTokenExpiresAtMs\":{expires}"));
    }
    if let Some(secret) = &payload.client_secret {
        text.push_str(",\"clientSecret\":");
        text.push_str(&serde_json::to_string(secret.as_str()).map_err(|_| corrupt())?);
    }
    text.push('}');
    Ok(SecretBytes(Zeroizing::new(text.as_bytes().to_vec())))
}

/// Unreserved characters stay literal; everything else is percent encoded.
const FORM_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn form(pairs: &[(&str, &str)]) -> Zeroizing<Vec<u8>> {
    let mut text = Zeroizing::new(String::new());
    for (index, (name, value)) in pairs.iter().enumerate() {
        if index > 0 {
            text.push('&');
        }
        for part in percent_encoding::utf8_percent_encode(name, FORM_SET) {
            text.push_str(part);
        }
        text.push('=');
        for part in percent_encoding::utf8_percent_encode(value, FORM_SET) {
            text.push_str(part);
        }
    }
    Zeroizing::new(text.as_bytes().to_vec())
}

/// Owns the encoded credential bytes so they are wiped once the request body
/// has been streamed.
struct SecretBody {
    data: Zeroizing<Vec<u8>>,
    offset: usize,
}
impl tokio::io::AsyncRead for SecretBody {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let remaining = self.data.len() - self.offset;
        let take = remaining.min(buffer.remaining());
        let offset = self.offset;
        buffer.put_slice(&self.data[offset..offset + take]);
        self.offset += take;
        std::task::Poll::Ready(Ok(()))
    }
}

async fn post_token(
    dependencies: &Dependencies,
    token_endpoint: url::Url,
    account: &AccountKey,
    body: Zeroizing<Vec<u8>>,
    cancel: &Cancellation,
) -> Result<TokenResponse> {
    let length = body.len() as u64;
    let mut headers = BTreeMap::new();
    headers.insert("content-type".to_owned(), FORM_CONTENT_TYPE.to_owned());
    let request = HttpRequest {
        method: reqwest::Method::POST,
        url: token_endpoint,
        headers,
        body: Some(Box::pin(SecretBody {
            data: body,
            offset: 0,
        })),
        content_length: Some(length),
        operation: ProviderOperation::Authenticate,
        account: account.clone(),
        api_request: true,
        mybox_charge: None,
        control: true,
    };
    let mut response = dependencies.send(request, cancel).await?;
    if response.status == 200 {
        return wire::json::<TokenResponse>(&mut response, cancel).await;
    }
    if matches!(response.status, 400 | 401) {
        // invalid_grant covers a revoked, replaced or testing-expired grant;
        // every other grant failure equally needs a new user authorization.
        return Err(ProviderError {
            kind: ErrorKind::ReauthRequired,
            http_status: Some(response.status),
            retry_at_ms: None,
        });
    }
    Err(wire::classify(&mut response, dependencies.clock.now_ms(), cancel).await)
}

pub(super) async fn verify_google_grant(
    dependencies: &Dependencies,
    token_info_endpoint: url::Url,
    expected_client_id: &str,
    required_scopes: &[String],
    access_token: &str,
    account: &AccountKey,
    cancel: &Cancellation,
) -> Result<()> {
    let mut url = token_info_endpoint;
    // Google OAuth2 API v2 documents access_token as a tokeninfo query
    // parameter. HttpRequest is intentionally neither Debug nor Serialize,
    // and transport errors discard URLs so this value cannot reach logs.
    url.query_pairs_mut()
        .append_pair("access_token", access_token);
    let request = HttpRequest {
        method: reqwest::Method::GET,
        url,
        headers: BTreeMap::new(),
        body: None,
        content_length: None,
        operation: ProviderOperation::Authenticate,
        account: account.clone(),
        api_request: true,
        mybox_charge: None,
        control: true,
    };
    let mut response = dependencies.send(request, cancel).await?;
    if matches!(response.status, 400 | 401) {
        return Err(reauth());
    }
    wire::require_status(&mut response, &[200], dependencies.clock.now_ms(), cancel).await?;
    let token = wire::json::<TokenInfo>(&mut response, cancel).await?;
    if token.issued_to.as_deref() != Some(expected_client_id) {
        return Err(reauth());
    }
    let granted_scopes = token.scope.as_deref().unwrap_or_default();
    if !required_scopes.iter().all(|required| {
        granted_scopes
            .split_ascii_whitespace()
            .any(|granted| granted == required)
    }) {
        return Err(reauth());
    }
    Ok(())
}

async fn account_id(
    dependencies: &Dependencies,
    settings: &AuthorizationSettings,
    access_token: &str,
    account: &AccountKey,
    cancel: &Cancellation,
) -> Result<String> {
    let mut url = settings.api("/about")?;
    url.query_pairs_mut()
        .append_pair("fields", "user(permissionId)");
    let mut headers = BTreeMap::new();
    headers.insert("authorization".to_owned(), format!("Bearer {access_token}"));
    let request = HttpRequest {
        method: reqwest::Method::GET,
        url,
        headers,
        body: None,
        content_length: None,
        operation: ProviderOperation::Metadata,
        account: account.clone(),
        api_request: true,
        mybox_charge: None,
        control: true,
    };
    let mut response = dependencies.send(request, cancel).await?;
    if response.status == 401 {
        return Err(reauth());
    }
    wire::require_status(&mut response, &[200], dependencies.clock.now_ms(), cancel).await?;
    let permission_id = wire::json::<About>(&mut response, cancel)
        .await?
        .user
        .and_then(|user| user.permission_id)
        .ok_or_else(reauth)?;
    if permission_id.len() > 128 || !config::is_drive_id(&permission_id) {
        return Err(reauth());
    }
    Ok(permission_id)
}

/// Returns a usable access token, refreshing and persisting rotation when the
/// cached and stored tokens are expired or `force_refresh` is set.
pub(super) async fn access_token(
    dependencies: &Dependencies,
    settings: &Settings,
    secret: &SecretRef,
    account: &AccountKey,
    cache: &Mutex<Option<CachedToken>>,
    force_refresh: bool,
    cancel: &Cancellation,
) -> Result<Zeroizing<String>> {
    let now_ms = dependencies.clock.now_ms();
    let fresh = |expires_at_ms: u64| expires_at_ms > now_ms.saturating_add(EXPIRY_SKEW_MS);
    if !force_refresh {
        let cached = cache
            .lock()
            .unwrap()
            .as_ref()
            .filter(|token| fresh(token.expires_at_ms))
            .map(CachedToken::duplicate);
        if let Some(token) = cached {
            return Ok(token.access);
        }
    }
    let payload = decode(&dependencies.vault.read(secret).await?)?;
    if !force_refresh {
        if let (Some(access), Some(expires_at_ms)) = (&payload.access_token, payload.expires_at_ms)
        {
            if fresh(expires_at_ms) {
                let token = CachedToken {
                    access: Zeroizing::new(access.to_string()),
                    expires_at_ms,
                };
                *cache.lock().unwrap() = Some(token.duplicate());
                return Ok(token.access);
            }
        }
    }
    let mut refresh_form = vec![
        ("client_id", settings.client_id.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", payload.refresh_token.as_str()),
    ];
    if let Some(client_secret) = &payload.client_secret {
        refresh_form.push(("client_secret", client_secret.as_str()));
    }
    let granted = post_token(
        dependencies,
        settings.token_endpoint()?,
        account,
        form(&refresh_form),
        cancel,
    )
    .await?;
    let access = granted
        .access_token
        .filter(|token| !token.is_empty())
        .map(Zeroizing::new)
        .ok_or_else(reauth)?;
    let expires_at_ms = dependencies
        .clock
        .now_ms()
        .saturating_add(granted.expires_in.unwrap_or(0).saturating_mul(1000));
    #[cfg(any(target_os = "android", target_os = "ios"))]
    verify_google_grant(
        dependencies,
        settings.token_info_endpoint()?,
        &settings.client_id,
        &settings.scopes(),
        access.as_str(),
        account,
        cancel,
    )
    .await?;
    let rotated = SecretPayload {
        refresh_token: granted
            .refresh_token
            .filter(|token| !token.is_empty())
            .map(Zeroizing::new)
            .unwrap_or(payload.refresh_token),
        access_token: Some(Zeroizing::new(access.to_string())),
        expires_at_ms: Some(expires_at_ms),
        client_secret: payload.client_secret,
    };
    dependencies
        .vault
        .replace(secret, &encode(&rotated)?)
        .await?;
    *cache.lock().unwrap() = Some(CachedToken {
        access: Zeroizing::new(access.to_string()),
        expires_at_ms,
    });
    Ok(access)
}

/// Installed-application policy for a user-owned OAuth client. `drive.file`
/// keeps the grant to files this application created, so no restricted scope
/// review is required and unrelated user files stay out of reach.
pub(crate) fn authorization_policy(
    config: &ConnectionConfig,
    platform: &str,
    redirect_url: url::Url,
) -> Result<AuthorizationPolicy> {
    if matches!(platform, "android" | "ios") {
        return Err(config::unsupported());
    }
    let settings = AuthorizationSettings::parse(config, platform)?;
    let loopback = redirect_url.scheme() == "http"
        && matches!(redirect_url.host_str(), Some("127.0.0.1") | Some("[::1]"));
    let custom_scheme =
        !matches!(redirect_url.scheme(), "http" | "https") && redirect_url.scheme().contains('.');
    if !loopback && !custom_scheme {
        return Err(config::unsupported());
    }
    Ok(AuthorizationPolicy {
        authorize_url: url::Url::parse(config::AUTHORIZE_ENDPOINT)
            .map_err(|_| config::unsupported())?,
        client_id: settings.client_id,
        redirect_url,
        scopes: settings.scopes,
    })
}

#[cfg(any(target_os = "ios", test))]
pub(crate) fn ios_authorization_policy(
    config: &ConnectionConfig,
) -> Result<(AuthorizationPolicy, String)> {
    const SUFFIX: &str = ".apps.googleusercontent.com";
    let settings = AuthorizationSettings::parse(config, "ios")?;
    let prefix = settings
        .client_id
        .strip_suffix(SUFFIX)
        .filter(|prefix| {
            !prefix.is_empty()
                && prefix
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
        .ok_or_else(config::unsupported)?;
    let callback_scheme = format!("com.googleusercontent.apps.{prefix}");
    let redirect_url = url::Url::parse(&format!("{callback_scheme}:/oauth2redirect"))
        .map_err(|_| config::unsupported())?;
    let policy = AuthorizationPolicy {
        authorize_url: url::Url::parse(config::AUTHORIZE_ENDPOINT)
            .map_err(|_| config::unsupported())?,
        client_id: settings.client_id,
        redirect_url,
        scopes: settings.scopes,
    };
    Ok((policy, callback_scheme))
}

#[cfg(not(target_os = "android"))]
pub(crate) fn native_authorization_policy(
    config: &ConnectionConfig,
    redirect_url: url::Url,
) -> Result<AuthorizationPolicy> {
    authorization_policy(config, config::platform_key(), redirect_url)
}

#[cfg(any(target_os = "android", test))]
pub(crate) fn android_web_authorization_policy(
    config: &ConnectionConfig,
) -> Result<AuthorizationPolicy> {
    let settings = AuthorizationSettings::parse(config, "android")?;
    Ok(AuthorizationPolicy {
        authorize_url: url::Url::parse(config::AUTHORIZE_ENDPOINT)
            .map_err(|_| config::unsupported())?,
        client_id: settings.client_id,
        redirect_url: settings
            .android_web_redirect
            .ok_or_else(config::unsupported)?,
        scopes: settings.scopes,
    })
}

/// Exchanges a PKCE grant for the secret payload the vault stores. The caller
/// owns `vault.store`, so one connection keeps one reference afterwards.
pub(crate) async fn exchange_authorization_code(
    dependencies: &Dependencies,
    config: &ConnectionConfig,
    grant: &AuthorizationCode,
    client_secret: Option<Zeroizing<String>>,
    cancel: &Cancellation,
) -> Result<AuthorizedSecret> {
    let settings = AuthorizationSettings::parse(config, config::platform_key())?;
    if grant.client_id != settings.client_id {
        return Err(reauth());
    }
    let pending = AccountKey::pending(config::PROVIDER_ID, &settings.api("/")?)?;
    let code = std::str::from_utf8(&grant.code.0).map_err(|_| reauth())?;
    let verifier = std::str::from_utf8(&grant.verifier.0).map_err(|_| reauth())?;
    if client_secret.as_ref().is_some_and(|secret| {
        secret.is_empty()
            || secret.len() > 4096
            || secret.trim() != secret.as_str()
            || secret.chars().any(char::is_control)
    }) {
        return Err(config::unsupported());
    }
    let mut exchange_form = vec![
        ("client_id", grant.client_id.as_str()),
        ("code", code),
        ("code_verifier", verifier),
        ("grant_type", "authorization_code"),
        ("redirect_uri", grant.redirect_url.as_str()),
    ];
    if let Some(secret) = &client_secret {
        exchange_form.push(("client_secret", secret.as_str()));
    }
    let granted = post_token(
        dependencies,
        settings.token_endpoint()?,
        &pending,
        form(&exchange_form),
        cancel,
    )
    .await?;
    let refresh_token = granted
        .refresh_token
        .filter(|token| !token.is_empty())
        .map(Zeroizing::new)
        .ok_or_else(reauth)?;
    let access_token = granted
        .access_token
        .filter(|token| !token.is_empty())
        .map(Zeroizing::new)
        .ok_or_else(reauth)?;
    let expires_at_ms = dependencies
        .clock
        .now_ms()
        .saturating_add(granted.expires_in.unwrap_or(0).saturating_mul(1000));
    #[cfg(any(target_os = "android", target_os = "ios"))]
    verify_google_grant(
        dependencies,
        settings.token_info_endpoint()?,
        &settings.client_id,
        &settings.scopes,
        &access_token,
        &pending,
        cancel,
    )
    .await?;
    let account_id = account_id(dependencies, &settings, &access_token, &pending, cancel).await?;
    let account = AccountKey::new(config::PROVIDER_ID, &settings.api("/")?, &account_id)?;
    dependencies.requests.resolve_pending(&pending, &account)?;
    let secret = encode(&SecretPayload {
        refresh_token,
        access_token: Some(access_token),
        expires_at_ms: Some(expires_at_ms),
        client_secret,
    })?;
    Ok(AuthorizedSecret { secret, account_id })
}
