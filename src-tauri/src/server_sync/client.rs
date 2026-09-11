use super::{Result, SyncError};
use reqwest::{blocking::Client, Method, Url};
use risunest_sync_wire::{canonical, RemoteHead, MAX_METADATA_BYTES};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{io::Read, time::Duration};

/// Credentials remain native local configuration, outside sync projections.
/// Deliberately no Debug implementation or URL/query credential transport.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ServerConfig {
    pub endpoint: String,
    pub library_id: String,
    pub device_id: String,
    pub token: String,
}
impl ServerConfig {
    pub fn validate(&self) -> Result<Url> {
        risunest_sync_wire::validate_id(&self.library_id)?;
        risunest_sync_wire::validate_id(&self.device_id)?;
        if self.token.len() != 64 || !self.token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(SyncError::new("invalid-device-token", 400));
        }
        let mut url =
            Url::parse(&self.endpoint).map_err(|_| SyncError::new("invalid-endpoint", 400))?;
        if !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(SyncError::new("invalid-endpoint", 400));
        }
        let loopback = matches!(url.host(),Some(url::Host::Ipv4(ip)) if ip.is_loopback())
            || matches!(url.host(),Some(url::Host::Ipv6(ip)) if ip.is_loopback())
            || url.host_str() == Some("localhost");
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            return Err(SyncError::new("https-required", 400));
        }
        let path = format!("{}/", url.path().trim_end_matches('/'));
        url.set_path(&path);
        Ok(url)
    }
}
pub(crate) struct ServerClient {
    http: Client,
    url: Url,
    config: ServerConfig,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}
pub(crate) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_range: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Identity {
    library_id: String,
    device_id: String,
    operation_watermark: risunest_sync_wire::Sequence,
    operation_pending: bool,
}
impl ServerClient {
    pub fn verify_identity(&self) -> Result<()> {
        self.identity().map(|_| ())
    }
    pub fn verify_new_identity(&self) -> Result<()> {
        let identity = self.identity()?;
        if identity.operation_watermark != 0.into() || identity.operation_pending {
            return Err(SyncError::new("new-device-registration-required", 409));
        }
        Ok(())
    }
    fn identity(&self) -> Result<Identity> {
        let (_, identity): (_, Identity) =
            self.json(Method::GET, "session", &[], None::<&()>, &[])?;
        if identity.library_id != self.config.library_id
            || identity.device_id != self.config.device_id
        {
            return Err(SyncError::new("device-identity-mismatch", 409));
        }
        Ok(identity)
    }
    pub fn new(config: ServerConfig) -> Result<Self> {
        Self::with_cancellation(config, None)
    }
    pub fn with_cancellation(
        config: ServerConfig,
        cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    ) -> Result<Self> {
        let url = config.validate()?;
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| SyncError::new("http-client-unavailable", 503))?;
        Ok(Self {
            http,
            url,
            config,
            cancelled,
        })
    }
    pub fn ensure_active(&self) -> Result<()> {
        if self
            .cancelled
            .as_ref()
            .is_some_and(|v| v.load(std::sync::atomic::Ordering::Acquire))
        {
            Err(SyncError::new("cancelled", 409))
        } else {
            Ok(())
        }
    }
    pub fn request(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<Vec<u8>>,
        headers: &[(&str, String)],
        limit: usize,
    ) -> Result<Reply> {
        self.ensure_active()?;
        if path.starts_with('/') || path.contains("..") || path.contains('?') || path.contains('#')
        {
            return Err(SyncError::new("invalid-request-path", 400));
        }
        let url = self
            .url
            .join(path)
            .map_err(|_| SyncError::new("invalid-request-path", 400))?;
        if url.origin() != self.url.origin() {
            return Err(SyncError::new("invalid-request-origin", 400));
        }
        let mut request = self
            .http
            .request(method, url)
            .query(query)
            .bearer_auth(&self.config.token)
            .header("x-risu-library", &self.config.library_id)
            .header("accept-encoding", "identity");
        if path == "objects/transfer"
            || path == "uploads/frames"
            || path.starts_with("object-deltas/")
            || (path.starts_with("uploads/") && path.ends_with("/delta"))
        {
            // Large recipes can approach 8 MiB. Include the 20-second job
            // wait plus transfer time at 1 Mbps; ordinary chunks stay 1 MiB.
            request = request.timeout(Duration::from_secs(120));
        }
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        if let Some(body) = body {
            if body.len() > 8 * 1024 * 1024 {
                return Err(SyncError::new("request-too-large", 413));
            }
            request = request.body(body);
        }
        let response = request.send().map_err(|e| {
            SyncError::new(
                if e.is_timeout() {
                    "server-timeout"
                } else {
                    "server-unreachable"
                },
                503,
            )
        })?;
        let status = response.status().as_u16();
        let content_range = response
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        if response
            .headers()
            .get("content-encoding")
            .is_some_and(|v| v != "identity")
        {
            return Err(SyncError::new("unexpected-content-encoding", 502));
        }
        if response.content_length().is_some_and(|v| v > limit as u64) {
            return Err(SyncError::new("response-too-large", 502));
        }
        let mut bytes = Vec::new();
        response
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| SyncError::new("incomplete-response", 503))?;
        if bytes.len() > limit {
            return Err(SyncError::new("response-too-large", 502));
        }
        Ok(Reply {
            status,
            body: bytes,
            content_range,
        })
    }
    pub fn json<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&impl Serialize>,
        headers: &[(&str, String)],
    ) -> Result<(u16, T)> {
        let reply = self.request(
            method,
            path,
            query,
            body.map(canonical::encode).transpose()?,
            headers,
            MAX_METADATA_BYTES,
        )?;
        if !(200..300).contains(&reply.status) {
            return Err(response_error(reply));
        }
        Ok((
            reply.status,
            canonical::decode(&reply.body, MAX_METADATA_BYTES)?,
        ))
    }
    pub fn head(&self) -> Result<RemoteHead> {
        let (_, head): (u16, RemoteHead) = self.json(Method::GET, "head", &[], None::<&()>, &[])?;
        head.validate()?;
        if head.library_id != self.config.library_id {
            return Err(SyncError::new("library-mismatch", 409));
        }
        Ok(head)
    }
}
pub(crate) fn response_error(reply: Reply) -> SyncError {
    let value: Option<serde_json::Value> = serde_json::from_slice(&reply.body).ok();
    let code = value
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(|v| v.as_str())
        .filter(|s| s.len() <= 64 && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
        .unwrap_or("server-response-error");
    SyncError::new(code, reply.status)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config(endpoint: &str) -> ServerConfig {
        ServerConfig {
            endpoint: endpoint.into(),
            library_id: "library".into(),
            device_id: "device".into(),
            token: "a".repeat(64),
        }
    }
    #[test]
    fn fixed_endpoint_requires_https_except_loopback_and_rejects_embedded_credentials() {
        for url in [
            "https://sync.example/base",
            "http://127.0.0.1:4319",
            "http://[::1]:4319",
        ] {
            assert!(config(url).validate().is_ok());
        }
        for url in [
            "http://192.168.0.1",
            "http://sync.example",
            "https://name:secret@sync.example",
            "https://sync.example/?token=x",
            "https://sync.example/#fragment",
            "file:///tmp",
        ] {
            assert!(config(url).validate().is_err());
        }
    }
}
