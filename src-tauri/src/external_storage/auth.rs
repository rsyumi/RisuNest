//! Provider policy is separate from common PKCE/state and native secret storage.
use super::contract::*;
use oauth2::{CsrfToken, PkceCodeChallenge, PkceCodeVerifier};

pub(crate) trait SecretVault: Send + Sync {
    fn read<'a>(&'a self, reference: &'a SecretRef) -> ProviderFuture<'a, SecretBytes>;
    fn store<'a>(&'a self, bytes: &'a SecretBytes) -> ProviderFuture<'a, SecretRef>;
    /// Rotated tokens overwrite the same reference so a connection keeps one secret.
    fn replace<'a>(
        &'a self,
        reference: &'a SecretRef,
        bytes: &'a SecretBytes,
    ) -> ProviderFuture<'a, ()>;
    fn remove<'a>(&'a self, reference: &'a SecretRef) -> ProviderFuture<'a, ()>;
}
// No serializer or debug formatter. The concrete vault owns OS sealing and namespace.
pub(crate) struct SecretBytes(pub zeroize::Zeroizing<Vec<u8>>);
pub(crate) struct AuthorizationPolicy {
    pub authorize_url: url::Url,
    pub client_id: String,
    pub redirect_url: url::Url,
    pub scopes: Vec<String>,
    pub picker: bool,
    /// The account a renewal has to sign in with, so the sign-in page selects it.
    pub login_hint: Option<String>,
}
impl AuthorizationPolicy {
    pub fn with_login_hint(mut self, login_hint: Option<String>) -> Self {
        self.login_hint = login_hint;
        self
    }
}
pub(crate) struct PendingAuthorization {
    policy: AuthorizationPolicy,
    state: CsrfToken,
    verifier: PkceCodeVerifier,
}
pub(crate) struct AuthorizationCode {
    pub code: SecretBytes,
    pub verifier: SecretBytes,
    pub client_id: String,
    pub redirect_url: url::Url,
    pub picked_file_id: Option<String>,
}
impl PendingAuthorization {
    pub fn start(policy: AuthorizationPolicy) -> Result<(Self, url::Url)> {
        if policy.authorize_url.scheme() != "https"
            || !policy.authorize_url.username().is_empty()
            || policy.authorize_url.password().is_some()
            || policy.authorize_url.query().is_some()
            || policy.authorize_url.fragment().is_some()
            || policy.client_id.is_empty()
            || policy.redirect_url.query().is_some()
            || policy.redirect_url.fragment().is_some()
        {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let state = CsrfToken::new_random();
        let mut authorize = policy.authorize_url.clone();
        authorize
            .query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &policy.client_id)
            .append_pair("redirect_uri", policy.redirect_url.as_str())
            .append_pair("scope", &policy.scopes.join(" "))
            .append_pair("state", state.secret())
            .append_pair("code_challenge", challenge.as_str())
            .append_pair("code_challenge_method", "S256");
        if let Some(login_hint) = policy.login_hint.as_deref().map(str::trim).filter(|hint| !hint.is_empty()) {
            authorize.query_pairs_mut().append_pair("login_hint", login_hint);
        }
        if policy.picker {
            authorize
                .query_pairs_mut()
                .append_pair("prompt", "consent")
                .append_pair("trigger_onepick", "true")
                .append_pair("allow_multiple", "false")
                .append_pair("allow_folder_selection", "true")
                .append_pair("mimetypes", "application/vnd.google-apps.folder");
        }
        Ok((
            Self {
                policy,
                state,
                verifier,
            },
            authorize,
        ))
    }
    pub fn matches_state(&self, callback: &url::Url) -> bool {
        let mut base = callback.clone();
        base.set_query(None);
        let mut states = callback.query_pairs().filter(|(name, _)| name == "state");
        base == self.policy.redirect_url
            && callback.fragment().is_none()
            && states.next().is_some_and(|(_, value)| value == *self.state.secret())
            && states.next().is_none()
    }

    pub fn finish(self, callback: &url::Url) -> Result<AuthorizationCode> {
        let mut base = callback.clone();
        base.set_query(None);
        if base != self.policy.redirect_url || callback.fragment().is_some() {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        let pairs: Vec<_> = callback.query_pairs().collect();
        let states: Vec<_> = pairs.iter().filter(|(name, _)| name == "state").collect();
        let codes: Vec<_> = pairs.iter().filter(|(name, _)| name == "code").collect();
        let errors: Vec<_> = pairs.iter().filter(|(name, _)| name == "error").collect();
        let picked: Vec<_> = pairs
            .iter()
            .filter(|(name, _)| name == "picked_file_ids")
            .collect();
        if states.len() != 1 || states[0].1 != *self.state.secret() {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        if !errors.is_empty() {
            if errors.len() != 1 || errors[0].1.is_empty() || !codes.is_empty() {
                return Err(ProviderError::new(ErrorKind::ReauthRequired));
            }
            return Err(ProviderError::new(if errors[0].1 == "access_denied" {
                ErrorKind::Cancelled
            } else {
                ErrorKind::ReauthRequired
            }));
        }
        if codes.len() != 1 || codes[0].1.is_empty() || codes[0].1.len() > 8192 {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        let picked_file_id = match picked.as_slice() {
            [] if !self.policy.picker => None,
            [value]
                if self.policy.picker
                    && !value.1.is_empty()
                    && value.1.len() <= 256
                    && !value.1.contains(',')
                    && value.1.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
                    }) =>
            {
                Some(value.1.to_string())
            }
            _ => return Err(ProviderError::new(ErrorKind::ReauthRequired)),
        };
        Ok(AuthorizationCode {
            code: SecretBytes(zeroize::Zeroizing::new(codes[0].1.as_bytes().into())),
            verifier: SecretBytes(zeroize::Zeroizing::new(
                self.verifier.secret().as_bytes().into(),
            )),
            client_id: self.policy.client_id,
            redirect_url: self.policy.redirect_url,
            picked_file_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn policy() -> AuthorizationPolicy {
        AuthorizationPolicy {
            authorize_url: url::Url::parse("https://synthetic.invalid/authorize").unwrap(),
            client_id: "user-owned-public-client".into(),
            redirect_url: url::Url::parse("risunest://oauth/callback").unwrap(),
            scopes: vec!["synthetic".into()],
            picker: false,
            login_hint: None,
        }
    }
    #[test]
    fn pkce_binds_redirect_and_single_use_state_without_client_secret() {
        let (pending, url) = PendingAuthorization::start(policy()).unwrap();
        let state = url
            .query_pairs()
            .find(|(name, _)| name == "state")
            .unwrap()
            .1
            .to_string();
        assert!(url
            .query_pairs()
            .any(|(name, value)| name == "code_challenge_method" && value == "S256"));
        assert!(!url.query_pairs().any(|(name, _)| name == "client_secret"));
        let mut callback = policy().redirect_url;
        callback
            .query_pairs_mut()
            .append_pair("state", &state)
            .append_pair("code", "synthetic-code");
        let grant = pending.finish(&callback).unwrap();
        assert_eq!(grant.code.0.as_slice(), b"synthetic-code");
        assert!(grant.verifier.0.len() >= 43);
        let (pending, _) = PendingAuthorization::start(policy()).unwrap();
        assert!(pending.finish(&callback).is_err());
        let (pending, url) = PendingAuthorization::start(policy()).unwrap();
        let state = url
            .query_pairs()
            .find(|(name, _)| name == "state")
            .unwrap()
            .1
            .to_string();
        let mut wrong = url::Url::parse("https://other.invalid/callback").unwrap();
        wrong
            .query_pairs_mut()
            .append_pair("state", &state)
            .append_pair("code", "synthetic-code");
        assert!(pending.finish(&wrong).is_err());

        let (pending, url) = PendingAuthorization::start(policy()).unwrap();
        let state = url
            .query_pairs()
            .find(|(name, _)| name == "state")
            .unwrap()
            .1
            .to_string();
        let mut denied = policy().redirect_url;
        denied
            .query_pairs_mut()
            .append_pair("state", &state)
            .append_pair("error", "access_denied");
        assert_eq!(
            pending.finish(&denied).err().unwrap().kind,
            ErrorKind::Cancelled
        );
    }

    #[test]
    fn picker_policy_requests_one_folder_and_binds_its_callback_id() {
        let mut picker = policy();
        picker.picker = true;
        let (pending, url) = PendingAuthorization::start(picker).unwrap();
        let pairs: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(
            pairs.get("trigger_onepick").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            pairs.get("allow_folder_selection").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            pairs.get("mimetypes").map(String::as_str),
            Some("application/vnd.google-apps.folder")
        );
        let mut callback = policy().redirect_url;
        callback
            .query_pairs_mut()
            .append_pair("state", pairs.get("state").unwrap())
            .append_pair("code", "synthetic-code")
            .append_pair("picked_file_ids", "folder_123");
        assert_eq!(
            pending.finish(&callback).unwrap().picked_file_id.as_deref(),
            Some("folder_123")
        );

        let mut picker = policy();
        picker.picker = true;
        let (pending, url) = PendingAuthorization::start(picker).unwrap();
        let state = url
            .query_pairs()
            .find(|(name, _)| name == "state")
            .unwrap()
            .1
            .into_owned();
        let mut callback = policy().redirect_url;
        callback
            .query_pairs_mut()
            .append_pair("state", &state)
            .append_pair("code", "synthetic-code");
        let error = match pending.finish(&callback) {
            Ok(_) => panic!("a picker callback without a selected folder must fail"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::ReauthRequired);
    }
}
