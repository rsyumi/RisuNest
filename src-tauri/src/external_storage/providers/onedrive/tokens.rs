//! Vault payloads and Microsoft identity platform v2.0 grants: the stored token
//! document, the sealed upload session, the authorization policy and the token
//! endpoint exchanges. Secret material stays inside `Zeroizing` values and never
//! reaches a log, an error or a DTO.

use super::{
    config::{self, Settings},
    graph,
};
use crate::external_storage::{
    auth::{AuthorizationCode, AuthorizationPolicy, SecretBytes},
    contract::{
        Cancellation, ConnectionConfig, ErrorKind, ProviderError, ProviderOperation, Result,
    },
    http::{HttpRequest, HttpResponse},
    quota::AccountKey,
};
use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use std::collections::BTreeMap;
use zeroize::Zeroizing;

/// An access token is refreshed this far before its expiry so a long request
/// does not start on a credential that dies mid flight.
const EXPIRY_SKEW_MS: u64 = 60_000;
const FORM: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn unsupported() -> ProviderError {
    ProviderError::new(ErrorKind::Unsupported)
}
fn reauth() -> ProviderError {
    ProviderError::new(ErrorKind::ReauthRequired)
}

/// Vault payload. No Debug, Display or Serialize implementation.
pub(super) struct StoredTokens {
    pub refresh_token: Zeroizing<String>,
    pub access_token: Option<Zeroizing<String>>,
    pub expires_at_ms: Option<u64>,
}

impl StoredTokens {
    pub(super) fn usable_access_token(&self, now_ms: u64) -> Option<Zeroizing<String>> {
        let expires_at = self.expires_at_ms?;
        let token = self.access_token.as_ref()?;
        (expires_at > now_ms.saturating_add(EXPIRY_SKEW_MS)).then(|| token.clone())
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StoredWire {
    refresh_token: String,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    access_token_expires_at_ms: Option<u64>,
}

pub(super) fn decode(bytes: &[u8]) -> Result<StoredTokens> {
    let wire: StoredWire = serde_json::from_slice(bytes).map_err(|_| reauth())?;
    if wire.refresh_token.is_empty() {
        return Err(reauth());
    }
    Ok(StoredTokens {
        refresh_token: Zeroizing::new(wire.refresh_token),
        access_token: wire
            .access_token
            .filter(|token| !token.is_empty())
            .map(Zeroizing::new),
        expires_at_ms: wire.access_token_expires_at_ms,
    })
}

pub(super) fn encode(tokens: &StoredTokens) -> Result<SecretBytes> {
    let mut out = Zeroizing::new(String::from("{\"refreshToken\":"));
    out.push_str(&quoted(&tokens.refresh_token)?);
    if let (Some(access), Some(expires)) = (&tokens.access_token, tokens.expires_at_ms) {
        out.push_str(",\"accessToken\":");
        out.push_str(&quoted(access)?);
        out.push_str(",\"accessTokenExpiresAtMs\":");
        out.push_str(&expires.to_string());
    }
    out.push('}');
    Ok(SecretBytes(Zeroizing::new(out.as_bytes().to_vec())))
}

fn quoted(value: &str) -> Result<Zeroizing<String>> {
    serde_json::to_string(value)
        .map(Zeroizing::new)
        .map_err(|_| ProviderError::new(ErrorKind::Corrupt))
}

/// Sealed upload session. The upload URL grants writes without a bearer token,
/// so it is stored through the vault and never in a plain journal.
pub(super) struct SealedSession {
    pub upload_url: Zeroizing<String>,
    pub object_id: String,
    pub byte_length: u64,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionWire {
    upload_url: String,
    object_id: String,
    byte_length: u64,
}

pub(super) fn decode_session(bytes: &[u8]) -> Result<SealedSession> {
    let wire: SessionWire =
        serde_json::from_slice(bytes).map_err(|_| ProviderError::new(ErrorKind::Corrupt))?;
    if wire.upload_url.is_empty() || wire.object_id.is_empty() {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    Ok(SealedSession {
        upload_url: Zeroizing::new(wire.upload_url),
        object_id: wire.object_id,
        byte_length: wire.byte_length,
    })
}

pub(super) fn encode_session(session: &SealedSession) -> Result<SecretBytes> {
    let mut out = Zeroizing::new(String::from("{\"uploadUrl\":"));
    out.push_str(&quoted(&session.upload_url)?);
    out.push_str(",\"objectId\":");
    out.push_str(&quoted(&session.object_id)?);
    out.push_str(",\"byteLength\":");
    out.push_str(&session.byte_length.to_string());
    out.push('}');
    Ok(SecretBytes(Zeroizing::new(out.as_bytes().to_vec())))
}

fn scope_value(settings: &Settings) -> String {
    settings.account_type.scopes().join(" ")
}

fn field(form: &mut Zeroizing<String>, name: &str, value: &str) {
    if !form.is_empty() {
        form.push('&');
    }
    form.push_str(name);
    form.push('=');
    form.push_str(&utf8_percent_encode(value, FORM).to_string());
}

pub(super) fn refresh_form(settings: &Settings, refresh_token: &str) -> Zeroizing<String> {
    let mut form = Zeroizing::new(String::new());
    field(&mut form, "client_id", &settings.client_id);
    field(&mut form, "grant_type", "refresh_token");
    field(&mut form, "scope", &scope_value(settings));
    field(&mut form, "refresh_token", refresh_token);
    form
}

pub(super) fn authorization_code_form(
    settings: &Settings,
    grant: &AuthorizationCode,
) -> Result<Zeroizing<String>> {
    let code = std::str::from_utf8(grant.code.0.as_slice()).map_err(|_| unsupported())?;
    let verifier = std::str::from_utf8(grant.verifier.0.as_slice()).map_err(|_| unsupported())?;
    let mut form = Zeroizing::new(String::new());
    field(&mut form, "client_id", &grant.client_id);
    field(&mut form, "grant_type", "authorization_code");
    field(&mut form, "scope", &scope_value(settings));
    field(&mut form, "code", code);
    field(&mut form, "redirect_uri", grant.redirect_url.as_str());
    field(&mut form, "code_verifier", verifier);
    Ok(form)
}

pub(super) fn token_request(
    settings: &Settings,
    form: Zeroizing<String>,
    account: AccountKey,
) -> Result<HttpRequest> {
    let url = url::Url::parse(&format!(
        "{}/{}/oauth2/v2.0/token",
        config::base(&settings.authority),
        config::encode_segment(&settings.tenant)
    ))
    .map_err(|_| unsupported())?;
    let body = form.as_bytes().to_vec();
    let mut headers = BTreeMap::new();
    headers.insert(
        "content-type".to_owned(),
        "application/x-www-form-urlencoded".to_owned(),
    );
    Ok(HttpRequest {
        method: reqwest::Method::POST,
        url,
        headers,
        content_length: Some(body.len() as u64),
        body: Some(Box::pin(std::io::Cursor::new(body))),
        operation: ProviderOperation::Authenticate,
        account,
        api_request: false,
        mybox_charge: None,
        control: true,
    })
}

#[derive(serde::Deserialize)]
struct GrantWire {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// Microsoft may return a rotated refresh token; the old one is discarded and
/// the connection keeps a single vault reference.
pub(super) async fn parse_grant(
    response: &mut HttpResponse,
    previous_refresh: Option<&Zeroizing<String>>,
    now_ms: u64,
    cancel: &Cancellation,
) -> Result<StoredTokens> {
    let grant: GrantWire = graph::json(response, cancel).await?;
    if grant.access_token.is_empty() {
        return Err(reauth());
    }
    let refresh_token = match grant.refresh_token.filter(|token| !token.is_empty()) {
        Some(rotated) => Zeroizing::new(rotated),
        None => previous_refresh.cloned().ok_or_else(reauth)?,
    };
    let expires_at_ms = grant
        .expires_in
        .map(|seconds| now_ms.saturating_add(seconds.saturating_mul(1000)));
    Ok(StoredTokens {
        refresh_token,
        access_token: Some(Zeroizing::new(grant.access_token)),
        expires_at_ms,
    })
}

pub(super) fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

/// The common PKCE helper adds the state, challenge and redirect binding.
pub(super) fn authorization_policy(
    config: &ConnectionConfig,
    platform: &str,
) -> Result<AuthorizationPolicy> {
    let settings = config::validate_authorization(config)?;
    let redirect_url = settings.redirect_uri.clone().ok_or_else(unsupported)?;
    let authorize_url = url::Url::parse(&format!(
        "{}/{}/oauth2/v2.0/authorize",
        config::base(&settings.authority),
        config::encode_segment(&settings.tenant)
    ))
    .map_err(|_| unsupported())?;
    Ok(AuthorizationPolicy {
        authorize_url,
        client_id: config::platform_client_id(config, platform)?,
        redirect_url,
        scopes: settings
            .account_type
            .scopes()
            .iter()
            .map(|scope| (*scope).to_owned())
            .collect(),
    })
}
