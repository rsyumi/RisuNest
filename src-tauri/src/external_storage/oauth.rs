//! Native OAuth transport boundaries.
//!
//! Desktop authorization uses an external browser and a loopback callback
//! bound before the authorization URL is created. Mobile OAuth returns through
//! a platform-owned authentication session or registered custom URI while Rust
//! retains the state, PKCE verifier and provider-registered redirect URI.
use super::{
    auth::{AuthorizationCode, AuthorizationPolicy, PendingAuthorization},
    contract::{Cancellation, ErrorKind, ProviderError, Result},
};

#[cfg(target_os = "android")]
pub(crate) const ANDROID_ONEDRIVE_REDIRECT_URI: &str = "risunestlocal://oauth/onedrive";
#[cfg(target_os = "android")]
pub(crate) const ANDROID_GOOGLE_DRIVE_REDIRECT_URI: &str = "risunestlocal://oauth/google-drive";

#[cfg(any(target_os = "ios", test))]
pub(crate) struct IosWebAuthenticationAuthorization {
    pending: PendingAuthorization,
    authorization_url: url::Url,
    callback_scheme: String,
}

#[cfg(any(target_os = "ios", test))]
impl IosWebAuthenticationAuthorization {
    pub(crate) fn start(
        policy: AuthorizationPolicy,
        callback_scheme: String,
        offline_access: bool,
    ) -> Result<Self> {
        if callback_scheme.is_empty()
            || callback_scheme != policy.redirect_url.scheme()
            || matches!(callback_scheme.as_str(), "http" | "https")
        {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        let (pending, mut authorization_url) = PendingAuthorization::start(policy)?;
        if offline_access {
            authorization_url
                .query_pairs_mut()
                .append_pair("access_type", "offline")
                .append_pair("prompt", "consent");
        }
        Ok(Self {
            pending,
            authorization_url,
            callback_scheme,
        })
    }

    fn finish(self, raw_callback: &str) -> Result<AuthorizationCode> {
        if raw_callback.is_empty() || raw_callback.len() > 16 * 1024 {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        let callback = url::Url::parse(raw_callback)
            .map_err(|_| ProviderError::new(ErrorKind::ReauthRequired))?;
        self.pending.finish(&callback)
    }
}

#[cfg(target_os = "ios")]
impl IosWebAuthenticationAuthorization {
    pub(crate) async fn authenticate(self, app: &tauri::AppHandle) -> Result<AuthorizationCode> {
        use tauri_plugin_ios_native::{IosNativeExt, WebAuthenticationOutcome};
        let native = app.ios_native().clone();
        let session = native.clone();
        let authorization_url = self.authorization_url.to_string();
        let callback_scheme = self.callback_scheme.clone();
        let outcome = tokio::select! {
            outcome = session.authenticate(&authorization_url, &callback_scheme, false) => outcome,
            _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
                native.cancel_authentication().await;
                return Err(ProviderError::new(ErrorKind::ReauthRequired));
            }
        };
        match outcome {
            WebAuthenticationOutcome::Callback(callback) => self.finish(&callback),
            WebAuthenticationOutcome::Cancelled => {
                Err(ProviderError::new(ErrorKind::Cancelled))
            }
            WebAuthenticationOutcome::Failed => {
                Err(ProviderError::new(ErrorKind::Transient))
            }
        }
    }
}

#[cfg(not(target_os = "android"))]
pub(crate) struct LoopbackAuthorization {
    listener: tokio::net::TcpListener,
    redirect_url: url::Url,
    pending: PendingAuthorization,
}

#[cfg(not(target_os = "android"))]
impl LoopbackAuthorization {
    pub(crate) async fn start(
        policy: impl FnOnce(url::Url) -> Result<AuthorizationPolicy>,
    ) -> Result<(Self, url::Url)> {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        let port = listener
            .local_addr()
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?
            .port();
        let redirect_url = url::Url::parse(&format!(
            "http://127.0.0.1:{port}/external-storage/oauth/callback"
        ))
        .map_err(|_| ProviderError::new(ErrorKind::Unsupported))?;
        let (pending, authorization_url) =
            PendingAuthorization::start(policy(redirect_url.clone())?)?;
        Ok((
            Self {
                listener,
                redirect_url,
                pending,
            },
            authorization_url,
        ))
    }

    pub(crate) async fn wait(self, cancel: &Cancellation) -> Result<AuthorizationCode> {
        let accepted = tokio::select! {
            result = self.listener.accept() => result.map_err(|_| ProviderError::new(ErrorKind::Transient))?,
            _ = cancel.cancelled() => return Err(ProviderError::new(ErrorKind::Cancelled)),
            _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
                return Err(ProviderError::new(ErrorKind::ReauthRequired));
            }
        };
        let (mut stream, peer) = accepted;
        if !peer.ip().is_loopback() {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        let callback = read_callback(&mut stream, &self.redirect_url, cancel).await;
        let success = callback.is_ok();
        write_browser_response(&mut stream, success).await;
        self.pending.finish(&callback?)
    }
}

#[cfg(not(target_os = "android"))]
async fn read_callback(
    stream: &mut tokio::net::TcpStream,
    redirect_url: &url::Url,
    cancel: &Cancellation,
) -> Result<url::Url> {
    use tokio::io::AsyncReadExt;
    const MAX_REQUEST_BYTES: usize = 16 * 1024;
    let mut request = Vec::with_capacity(1024);
    loop {
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if request.len() == MAX_REQUEST_BYTES {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        let mut block = [0_u8; 1024];
        let remaining = MAX_REQUEST_BYTES - request.len();
        let read_limit = remaining.min(block.len());
        let read = tokio::select! {
            result = stream.read(&mut block[..read_limit]) => {
                result.map_err(|_| ProviderError::new(ErrorKind::Transient))?
            },
            _ = cancel.cancelled() => return Err(ProviderError::new(ErrorKind::Cancelled)),
        };
        if read == 0 {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        request.extend_from_slice(&block[..read]);
    }
    let line_end = request
        .windows(2)
        .position(|window| window == b"\r\n")
        .ok_or_else(|| ProviderError::new(ErrorKind::ReauthRequired))?;
    let line = std::str::from_utf8(&request[..line_end])
        .map_err(|_| ProviderError::new(ErrorKind::ReauthRequired))?;
    let mut parts = line.split(' ');
    let method = parts.next();
    let target = parts.next();
    let protocol = parts.next();
    if method != Some("GET")
        || !matches!(protocol, Some("HTTP/1.0") | Some("HTTP/1.1"))
        || parts.next().is_some()
    {
        return Err(ProviderError::new(ErrorKind::ReauthRequired));
    }
    let target = target.ok_or_else(|| ProviderError::new(ErrorKind::ReauthRequired))?;
    if !target.starts_with('/') || target.starts_with("//") || target.contains('#') {
        return Err(ProviderError::new(ErrorKind::ReauthRequired));
    }
    let origin = format!(
        "{}://{}:{}",
        redirect_url.scheme(),
        redirect_url.host_str().unwrap_or("127.0.0.1"),
        redirect_url.port().unwrap_or(80)
    );
    let callback = url::Url::parse(&format!("{origin}{target}"))
        .map_err(|_| ProviderError::new(ErrorKind::ReauthRequired))?;
    let mut base = callback.clone();
    base.set_query(None);
    if base != *redirect_url {
        return Err(ProviderError::new(ErrorKind::ReauthRequired));
    }
    Ok(callback)
}

#[cfg(not(target_os = "android"))]
async fn write_browser_response(stream: &mut tokio::net::TcpStream, success: bool) {
    use tokio::io::AsyncWriteExt;
    let (status, message) = if success {
        (
            "200 OK",
            "Authorization received. You can return to RisuNest.",
        )
    } else {
        (
            "400 Bad Request",
            "Authorization could not be accepted. Return to RisuNest and retry.",
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nCache-Control: no-store\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{message}",
        message.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(target_os = "android")]
pub(crate) struct AndroidRedirectAuthorization {
    state: String,
    receiver: Option<tokio::sync::oneshot::Receiver<Result<AuthorizationCode>>>,
}

#[cfg(target_os = "android")]
impl AndroidRedirectAuthorization {
    /// Supplies the original registered HTTPS callback for a manual fallback.
    /// The pending authorization remains usable when the URL belongs to a
    /// different or stale state.
    pub(crate) fn submit_callback(&self, redirect_url: &str) -> Result<()> {
        android::complete_redirect(redirect_url, Some(&self.state))
    }

    /// Returns without consuming the flow when the browser callback has not
    /// arrived. Invalid manual input also leaves the current attempt usable.
    pub(crate) fn try_complete(
        &mut self,
        redirect_url: Option<&str>,
    ) -> Result<Option<AuthorizationCode>> {
        if redirect_url.is_some() {
            if let Some(grant) = self.take_ready()? {
                return Ok(Some(grant));
            }
        }
        if let Some(redirect_url) = redirect_url {
            self.submit_callback(redirect_url)?;
        }
        self.take_ready()
    }

    pub(crate) fn is_consumed(&self) -> bool {
        self.receiver.is_none()
    }

    fn take_ready(&mut self) -> Result<Option<AuthorizationCode>> {
        let receiver = self
            .receiver
            .as_mut()
            .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        match receiver.try_recv() {
            Ok(result) => {
                self.receiver.take();
                result.map(Some)
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => Ok(None),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                self.receiver.take();
                Err(ProviderError::new(ErrorKind::Transient))
            }
        }
    }

    pub(crate) async fn wait(mut self, cancel: &Cancellation) -> Result<AuthorizationCode> {
        let receiver = self
            .receiver
            .take()
            .ok_or_else(|| ProviderError::new(ErrorKind::Corrupt))?;
        tokio::select! {
            result = receiver => result.unwrap_or_else(|_| Err(ProviderError::new(ErrorKind::Transient))),
            _ = cancel.cancelled() => {
                android::remove_redirect(&self.state);
                Err(ProviderError::new(ErrorKind::Cancelled))
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {
                android::remove_redirect(&self.state);
                Err(ProviderError::new(ErrorKind::ReauthRequired))
            }
        }
    }
}

#[cfg(target_os = "android")]
impl Drop for AndroidRedirectAuthorization {
    fn drop(&mut self) {
        if self.receiver.is_some() {
            android::remove_redirect(&self.state);
        }
    }
}

#[cfg(any(target_os = "android", test))]
enum AndroidCallback {
    Grant(url::Url),
    OAuthError(ErrorKind),
}

#[cfg(any(target_os = "android", test))]
fn validate_android_callback(
    state: &str,
    registered_redirect: &url::Url,
    app_redirect: &url::Url,
    callback: &url::Url,
) -> Result<AndroidCallback> {
    if callback.fragment().is_some() {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let mut base = callback.clone();
    base.set_query(None);
    let registered = base == *registered_redirect;
    if !registered && base != *app_redirect {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let pairs = callback.query_pairs().collect::<Vec<_>>();
    let states = pairs
        .iter()
        .filter(|(name, _)| name == "state")
        .collect::<Vec<_>>();
    let codes = pairs
        .iter()
        .filter(|(name, _)| name == "code")
        .collect::<Vec<_>>();
    let errors = pairs
        .iter()
        .filter(|(name, _)| name == "error")
        .collect::<Vec<_>>();
    if states.len() != 1 || states[0].1 != state {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    if !errors.is_empty() {
        if errors.len() != 1 || errors[0].1.is_empty() || !codes.is_empty() {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        return Ok(AndroidCallback::OAuthError(
            if errors[0].1 == "access_denied" {
                ErrorKind::Cancelled
            } else {
                ErrorKind::ReauthRequired
            },
        ));
    }
    if codes.len() != 1 || codes[0].1.is_empty() || codes[0].1.len() > 8192 {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    if registered {
        Ok(AndroidCallback::Grant(callback.clone()))
    } else {
        let mut normalized = registered_redirect.clone();
        normalized.set_query(callback.query());
        Ok(AndroidCallback::Grant(normalized))
    }
}

#[cfg(target_os = "android")]
mod android {
    use super::*;
    use jni::{
        objects::{JClass, JString},
        JNIEnv,
    };
    use std::{
        collections::HashMap,
        sync::{Mutex, OnceLock},
    };
    use tokio::sync::oneshot;

    struct RedirectPending {
        authorization: PendingAuthorization,
        registered_redirect: url::Url,
        app_redirect: url::Url,
        sender: oneshot::Sender<Result<AuthorizationCode>>,
    }

    static REDIRECTS: OnceLock<Mutex<HashMap<String, RedirectPending>>> = OnceLock::new();

    fn redirects() -> &'static Mutex<HashMap<String, RedirectPending>> {
        REDIRECTS.get_or_init(|| Mutex::new(HashMap::new()))
    }

    pub(super) fn insert_redirect(
        state: String,
        authorization: PendingAuthorization,
        registered_redirect: url::Url,
        app_redirect: url::Url,
        sender: oneshot::Sender<Result<AuthorizationCode>>,
    ) -> Result<()> {
        let mut pending = redirects()
            .lock()
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        if pending.len() >= 8 || pending.contains_key(&state) {
            return Err(ProviderError::new(ErrorKind::Transient));
        }
        pending.insert(
            state,
            RedirectPending {
                authorization,
                registered_redirect,
                app_redirect,
                sender,
            },
        );
        Ok(())
    }

    pub(super) fn remove_redirect(state: &str) {
        redirects()
            .lock()
            .ok()
            .and_then(|mut map| map.remove(state));
    }

    pub(super) fn complete_redirect(raw: &str, expected_state: Option<&str>) -> Result<()> {
        if raw.is_empty() || raw.len() > 16 * 1024 {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        let callback =
            url::Url::parse(raw).map_err(|_| ProviderError::new(ErrorKind::ReauthRequired))?;
        let states = callback
            .query_pairs()
            .filter(|(name, _)| name == "state")
            .map(|(_, value)| value.into_owned())
            .collect::<Vec<_>>();
        if states.len() != 1 || states[0].is_empty() || states[0].len() > 1024 {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        let state = &states[0];
        if expected_state.is_some_and(|expected| expected != state) {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        let mut redirects = redirects()
            .lock()
            .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
        let pending = redirects.get(state).ok_or_else(|| {
            ProviderError::new(if expected_state.is_some() {
                ErrorKind::Unsupported
            } else {
                ErrorKind::NotFound
            })
        })?;
        let outcome = validate_android_callback(
            state,
            &pending.registered_redirect,
            &pending.app_redirect,
            &callback,
        )?;
        let pending = redirects
            .remove(state)
            .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
        drop(redirects);
        let RedirectPending {
            authorization,
            sender,
            ..
        } = pending;
        let result = match outcome {
            AndroidCallback::Grant(callback) => authorization.finish(&callback),
            AndroidCallback::OAuthError(kind) => Err(ProviderError::new(kind)),
        };
        // The entry was removed before finishing, so a callback is single-use
        // even if validation or delivery fails.
        sender
            .send(result)
            .map_err(|_| ProviderError::new(ErrorKind::NotFound))
    }

    #[no_mangle]
    pub extern "system" fn Java_io_github_rsyumi_risunest_ExternalStorageAuthorization_completeOAuthRedirectNative(
        mut env: JNIEnv,
        _class: JClass,
        redirect_url: JString,
    ) {
        let delivered = (|| {
            let raw: String = env.get_string(&redirect_url).ok()?.into();
            complete_redirect(&raw, None).ok()
        })();
        if delivered.is_none() {
            let _ = env.exception_clear();
        }
    }
}

#[cfg(target_os = "android")]
pub(crate) fn android_redirect_authorization(
    policy: AuthorizationPolicy,
) -> Result<(AndroidRedirectAuthorization, url::Url)> {
    if policy.redirect_url.as_str() != ANDROID_ONEDRIVE_REDIRECT_URI {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    start_android_redirect(policy, ANDROID_ONEDRIVE_REDIRECT_URI, false)
}

#[cfg(target_os = "android")]
pub(crate) fn android_google_web_authorization(
    policy: AuthorizationPolicy,
) -> Result<(AndroidRedirectAuthorization, url::Url)> {
    if policy.redirect_url.scheme() != "https" {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    start_android_redirect(policy, ANDROID_GOOGLE_DRIVE_REDIRECT_URI, true)
}

#[cfg(target_os = "android")]
fn start_android_redirect(
    policy: AuthorizationPolicy,
    app_redirect: &str,
    offline_access: bool,
) -> Result<(AndroidRedirectAuthorization, url::Url)> {
    let registered_redirect = policy.redirect_url.clone();
    let app_redirect =
        url::Url::parse(app_redirect).map_err(|_| ProviderError::new(ErrorKind::Unsupported))?;
    if app_redirect.query().is_some() || app_redirect.fragment().is_some() {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    let (pending, mut authorization_url) = PendingAuthorization::start(policy)?;
    if offline_access {
        authorization_url
            .query_pairs_mut()
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent");
    }
    let states = authorization_url
        .query_pairs()
        .filter(|(name, _)| name == "state")
        .map(|(_, value)| value.into_owned())
        .collect::<Vec<_>>();
    if states.len() != 1 || states[0].is_empty() || states[0].len() > 1024 {
        return Err(ProviderError::new(ErrorKind::Corrupt));
    }
    let state = states.into_iter().next().unwrap_or_default();
    let (sender, receiver) = tokio::sync::oneshot::channel();
    android::insert_redirect(
        state.clone(),
        pending,
        registered_redirect,
        app_redirect,
        sender,
    )?;
    Ok((
        AndroidRedirectAuthorization {
            state,
            receiver: Some(receiver),
        },
        authorization_url,
    ))
}

#[cfg(all(test, not(target_os = "android")))]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn policy(redirect_url: url::Url) -> Result<AuthorizationPolicy> {
        Ok(AuthorizationPolicy {
            authorize_url: url::Url::parse("https://synthetic.invalid/authorize").unwrap(),
            client_id: "synthetic-public-client".into(),
            redirect_url,
            scopes: vec!["synthetic.scope".into()],
        })
    }

    #[test]
    fn loopback_callback_accepts_the_bound_path_and_state() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (pending, authorization_url) = LoopbackAuthorization::start(policy).await.unwrap();
            let state = authorization_url
                .query_pairs()
                .find(|(name, _)| name == "state")
                .unwrap()
                .1
                .to_string();
            let redirect = authorization_url
                .query_pairs()
                .find(|(name, _)| name == "redirect_uri")
                .unwrap()
                .1
                .to_string();
            let redirect = url::Url::parse(&redirect).unwrap();
            let task =
                tokio::spawn(async move { pending.wait(&Cancellation::default()).await.unwrap() });
            let mut stream =
                tokio::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap()))
                    .await
                    .unwrap();
            let target = format!("{}?state={state}&code=synthetic-code", redirect.path());
            stream
                .write_all(format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").as_bytes())
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 200 OK"));
            assert_eq!(task.await.unwrap().code.0.as_slice(), b"synthetic-code");
        });
    }

    #[test]
    fn android_app_callback_is_normalized_to_the_registered_https_redirect() {
        let registered = url::Url::parse("https://oauth.example.test/callback").unwrap();
        let app = url::Url::parse("risunestlocal://oauth/google-drive").unwrap();
        let (pending, authorization_url) =
            PendingAuthorization::start(policy(registered.clone()).unwrap()).unwrap();
        let state = authorization_url
            .query_pairs()
            .find(|(name, _)| name == "state")
            .unwrap()
            .1
            .into_owned();
        let callback = url::Url::parse(&format!(
            "risunestlocal://oauth/google-drive?code=synthetic-code&state={state}"
        ))
        .unwrap();
        let normalized =
            match validate_android_callback(&state, &registered, &app, &callback).unwrap() {
                AndroidCallback::Grant(callback) => callback,
                AndroidCallback::OAuthError(_) => panic!("expected grant"),
            };
        assert_eq!(
            normalized.as_str(),
            format!("https://oauth.example.test/callback?code=synthetic-code&state={state}")
        );
        let grant = pending.finish(&normalized).unwrap();
        assert_eq!(grant.redirect_url, registered);
        assert_eq!(grant.code.0.as_slice(), b"synthetic-code");
    }

    #[test]
    fn android_callback_rejects_bad_manual_input_before_consuming_the_flow() {
        let registered = url::Url::parse("https://oauth.example.test/callback").unwrap();
        let app = url::Url::parse("risunestlocal://oauth/google-drive").unwrap();
        for callback in [
            "https://other.example.test/callback?code=x&state=expected",
            "https://oauth.example.test/callback?code=x&state=stale",
            "https://oauth.example.test/callback?code=x&code=y&state=expected",
            "https://oauth.example.test/callback?code=x&state=expected#fragment",
        ] {
            assert_eq!(
                validate_android_callback(
                    "expected",
                    &registered,
                    &app,
                    &url::Url::parse(callback).unwrap(),
                )
                .err()
                .unwrap()
                .kind,
                ErrorKind::Unsupported
            );
        }
        let denied = url::Url::parse(
            "risunestlocal://oauth/google-drive?error=access_denied&state=expected",
        )
        .unwrap();
        match validate_android_callback("expected", &registered, &app, &denied).unwrap() {
            AndroidCallback::OAuthError(kind) => assert_eq!(kind, ErrorKind::Cancelled),
            AndroidCallback::Grant(_) => panic!("expected denial"),
        }
    }

    #[test]
    fn ios_authentication_session_binds_the_runtime_callback_scheme() {
        let redirect =
            url::Url::parse("com.googleusercontent.apps.123-client:/oauth2redirect").unwrap();
        let flow = IosWebAuthenticationAuthorization::start(
            policy(redirect.clone()).unwrap(),
            "com.googleusercontent.apps.123-client".into(),
            true,
        )
        .unwrap();
        assert_eq!(
            flow.callback_scheme,
            "com.googleusercontent.apps.123-client"
        );
        assert!(flow
            .authorization_url
            .query_pairs()
            .any(|(name, value)| name == "access_type" && value == "offline"));
        let state = flow
            .authorization_url
            .query_pairs()
            .find(|(name, _)| name == "state")
            .unwrap()
            .1
            .into_owned();
        let callback = format!("{redirect}?code=synthetic-code&state={state}");
        let grant = flow.finish(&callback).unwrap();
        assert_eq!(grant.redirect_url, redirect);
        assert_eq!(grant.code.0.as_slice(), b"synthetic-code");

        assert!(IosWebAuthenticationAuthorization::start(
            policy(url::Url::parse("risunestlocal://oauth/onedrive").unwrap()).unwrap(),
            "another-scheme".into(),
            false,
        )
        .is_err());
    }

    #[test]
    fn loopback_callback_rejects_wrong_paths_before_granting() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let (pending, authorization_url) = LoopbackAuthorization::start(policy).await.unwrap();
            let redirect = url::Url::parse(
                &authorization_url
                    .query_pairs()
                    .find(|(name, _)| name == "redirect_uri")
                    .unwrap()
                    .1,
            )
            .unwrap();
            let task = tokio::spawn(async move { pending.wait(&Cancellation::default()).await });
            let mut stream =
                tokio::net::TcpStream::connect(("127.0.0.1", redirect.port().unwrap()))
                    .await
                    .unwrap();
            stream
                .write_all(b"GET /wrong?code=a&state=b HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .await
                .unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).await.unwrap();
            assert!(response.starts_with("HTTP/1.1 400 Bad Request"));
            let result = task.await.unwrap();
            assert!(matches!(
                result,
                Err(ProviderError {
                    kind: ErrorKind::ReauthRequired,
                    ..
                })
            ));
        });
    }
}
