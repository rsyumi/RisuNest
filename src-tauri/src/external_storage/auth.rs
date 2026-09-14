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
        Ok((
            Self {
                policy,
                state,
                verifier,
            },
            authorize,
        ))
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
        if states.len() != 1
            || states[0].1 != *self.state.secret()
            || codes.len() != 1
            || codes[0].1.is_empty()
            || codes[0].1.len() > 8192
            || pairs.iter().any(|(name, _)| name == "error")
        {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        Ok(AuthorizationCode {
            code: SecretBytes(zeroize::Zeroizing::new(codes[0].1.as_bytes().into())),
            verifier: SecretBytes(zeroize::Zeroizing::new(
                self.verifier.secret().as_bytes().into(),
            )),
            client_id: self.policy.client_id,
            redirect_url: self.policy.redirect_url,
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
    }
}
