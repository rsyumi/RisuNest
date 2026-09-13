use super::{Result, SyncError};
use reqwest::{blocking::Client, Method, Url};
use risunest_sync_wire::{canonical, RemoteHead, MAX_METADATA_BYTES};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{io::Read, time::Duration};

pub(crate) use risunest_sync_connect::Registration as ServerConfig;
pub(crate) struct ServerClient {
    http: Client,
    url: Url,
    config: ServerConfig,
    cancelled: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) verified_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}
pub(crate) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    pub content_range: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Identity {
    head: risunest_sync_wire::RemoteHead,
    device_id: String,
    operation_watermark: risunest_sync_wire::Sequence,
    operation_pending: bool,
}
impl ServerClient {
    /// Resolve only at an identity GET boundary. Never replay a mutation on a new URL.
    pub fn resolve_identity(&mut self, new_registration: bool) -> Result<RemoteHead> {
        let verify = |client: &Self| -> Result<RemoteHead> {
            let identity = client.identity()?;
            if new_registration
                && (identity.operation_watermark != 0.into() || identity.operation_pending)
            {
                return Err(SyncError::new("new-device-registration-required", 409));
            }
            Ok(identity.head)
        };
        let original = match verify(self) {
            Ok(head) => return Ok(head),
            Err(error) => error,
        };
        if matches!(
            original.code.as_str(),
            "cancelled" | "new-device-registration-required"
        ) {
            return Err(original);
        }
        let Some(directory) = &self.config.directory else {
            return Err(original);
        };
        self.ensure_active()?;
        // A separate unauthenticated request. Device and library headers are never sent.
        let response = self
            .http
            .get(directory.record_url()?)
            .header("accept-encoding", "identity")
            .header("cache-control", "no-cache")
            .timeout(Duration::from_secs(15))
            .send()
            .map_err(|_| SyncError::new("directory-unreachable", 503))?;
        if response.status().as_u16() != 200 {
            return Err(SyncError::new("directory-response-error", 503));
        }
        let limit = risunest_sync_connect::MAX_ENVELOPE_TEXT;
        if response
            .headers()
            .get("content-encoding")
            .is_some_and(|v| v != "identity")
            || response.content_length().is_some_and(|v| v > limit as u64)
        {
            return Err(SyncError::new("invalid-directory-envelope", 502));
        }
        let mut bytes = Vec::new();
        response
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| SyncError::new("directory-incomplete-response", 503))?;
        if bytes.len() > limit {
            return Err(SyncError::new("invalid-directory-envelope", 502));
        }
        let envelope = std::str::from_utf8(&bytes)
            .map_err(|_| SyncError::new("invalid-directory-envelope", 502))?;
        let endpoint =
            risunest_sync_connect::open_endpoint(&directory.uuid, &directory.key, envelope)?;
        if risunest_sync_connect::validate_endpoint(&endpoint, false)? == self.url {
            return Err(original);
        }
        self.ensure_active()?;
        let mut config = self.config.clone();
        config.endpoint = endpoint;
        let mut candidate = Self::with_cancellation(config, self.cancelled.clone())?;
        candidate.http = self.http.clone();
        candidate.verified_bytes = self.verified_bytes.clone();
        let head = verify(&candidate)?;
        *self = candidate;
        Ok(head)
    }
    pub fn config(&self) -> &ServerConfig {
        &self.config
    }
    fn identity(&self) -> Result<Identity> {
        let (_, identity): (_, Identity) =
            self.json(Method::GET, "session", &[], None::<&()>, &[])?;
        identity.head.validate()?;
        if identity.head.library_id != self.config.library_id
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
            verified_bytes: None,
        })
    }
    pub fn verified(&self, bytes: u64) {
        if let Some(counter) = &self.verified_bytes {
            counter.fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        }
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
            directory: None,
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

#[cfg(test)]
#[path = "directory_tests.rs"]
mod directory_tests;
