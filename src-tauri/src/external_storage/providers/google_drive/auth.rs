//! Secret payload, refresh-token rotation and the installed-application
//! authorization policy. Token material never leaves this module except as an
//! `Authorization` header value or a sealed vault payload.
use super::{
    config::{self, Settings},
    wire::{self, AccountScope},
};
use crate::external_storage::{
    auth::{AuthorizationCode, AuthorizationPolicy, SecretBytes},
    contract::*,
    http::{self, HttpRequest},
    providers::Dependencies,
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
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredSecret {
    refresh_token: String,
    access_token: Option<String>,
    access_token_expires_at_ms: Option<u64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    refresh_token: Option<String>,
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
    scope: &AccountScope,
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
        costs: wire::costs(ProviderOperation::Authenticate, scope, 0),
    };
    let mut response = http::send(
        dependencies.http.as_ref(),
        dependencies.budget.as_ref(),
        dependencies.clock.as_ref(),
        request,
        cancel,
    )
    .await?;
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

/// Returns a usable access token, refreshing and persisting rotation when the
/// cached and stored tokens are expired or `force_refresh` is set.
pub(super) async fn access_token(
    dependencies: &Dependencies,
    settings: &Settings,
    secret: &SecretRef,
    scope: &AccountScope,
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
    let granted = post_token(
        dependencies,
        settings.token_endpoint()?,
        scope,
        form(&[
            ("client_id", &settings.client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", payload.refresh_token.as_str()),
        ]),
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
    let rotated = SecretPayload {
        refresh_token: granted
            .refresh_token
            .filter(|token| !token.is_empty())
            .map(Zeroizing::new)
            .unwrap_or(payload.refresh_token),
        access_token: Some(Zeroizing::new(access.to_string())),
        expires_at_ms: Some(expires_at_ms),
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
    if config.provider != config::PROVIDER_ID {
        return Err(config::unsupported());
    }
    let profile = config
        .oauth_profile
        .as_ref()
        .ok_or_else(config::unsupported)?;
    let client_id = profile
        .platform_client_ids
        .get(platform)
        .filter(|client_id| !client_id.is_empty())
        .ok_or_else(config::unsupported)?
        .clone();
    let loopback = redirect_url.scheme() == "http"
        && matches!(redirect_url.host_str(), Some("127.0.0.1") | Some("[::1]"));
    let custom_scheme =
        !matches!(redirect_url.scheme(), "http" | "https") && redirect_url.scheme().contains('.');
    if !loopback && !custom_scheme {
        return Err(config::unsupported());
    }
    let scopes = match config.location.get("space").map(String::as_str) {
        None | Some("drive") => vec![config::DRIVE_FILE_SCOPE.to_owned()],
        Some(config::APP_DATA_FOLDER) => vec![config::APPDATA_SCOPE.to_owned()],
        Some(_) => return Err(config::unsupported()),
    };
    Ok(AuthorizationPolicy {
        authorize_url: url::Url::parse(config::AUTHORIZE_ENDPOINT)
            .map_err(|_| config::unsupported())?,
        client_id,
        redirect_url,
        scopes,
    })
}

/// Exchanges a PKCE grant for the secret payload the vault stores. The caller
/// owns `vault.store`, so one connection keeps one reference afterwards.
pub(crate) async fn exchange_authorization_code(
    dependencies: &Dependencies,
    config: &ConnectionConfig,
    grant: &AuthorizationCode,
    cancel: &Cancellation,
) -> Result<SecretBytes> {
    let settings = Settings::parse(config)?;
    let scope = AccountScope::of(&settings);
    let code = std::str::from_utf8(&grant.code.0).map_err(|_| reauth())?;
    let verifier = std::str::from_utf8(&grant.verifier.0).map_err(|_| reauth())?;
    let granted = post_token(
        dependencies,
        settings.token_endpoint()?,
        &scope,
        form(&[
            ("client_id", &grant.client_id),
            ("code", code),
            ("code_verifier", verifier),
            ("grant_type", "authorization_code"),
            ("redirect_uri", grant.redirect_url.as_str()),
        ]),
        cancel,
    )
    .await?;
    let refresh_token = granted
        .refresh_token
        .filter(|token| !token.is_empty())
        .map(Zeroizing::new)
        .ok_or_else(reauth)?;
    let expires_at_ms = dependencies
        .clock
        .now_ms()
        .saturating_add(granted.expires_in.unwrap_or(0).saturating_mul(1000));
    encode(&SecretPayload {
        refresh_token,
        access_token: granted
            .access_token
            .filter(|token| !token.is_empty())
            .map(Zeroizing::new),
        expires_at_ms: Some(expires_at_ms),
    })
}
