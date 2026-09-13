use crate::Result;
use risunest_sync_server::management::discovery::Discovery;
use serde_json::Value;
use std::{path::PathBuf, time::Duration};

pub struct Client {
    root: PathBuf,
    http: reqwest::Client,
}
impl Client {
    pub fn new(root: PathBuf) -> Result<Self> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| "management-client-unavailable")?;
        Ok(Self { root, http })
    }
    pub async fn status(&self) -> Result<Value> {
        self.send("status", None).await
    }
    pub async fn mutate(&self, path: &str, body: Value) -> Result<Value> {
        let allowed = [
            "connection",
            "devices",
            "registry/repost",
            "shutdown",
            "tunnel/start",
            "tunnel/stop",
            "tunnel/restart",
        ]
        .contains(&path)
            || path
                .strip_prefix("devices/")
                .and_then(|s| s.strip_suffix("/revoke"))
                .is_some_and(|id| {
                    !id.is_empty()
                        && id.len() <= 128
                        && id
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
                });
        if !allowed {
            return Err("invalid-management-action".into());
        }
        self.send(path, Some(body)).await
    }
    async fn send(&self, path: &str, body: Option<Value>) -> Result<Value> {
        // Reload only for each new user operation; never retry a mutation automatically.
        let locator = Discovery::load(&self.root).map_err(|e| e.code.to_owned())?;
        let url = format!("http://{}/{path}", locator.address);
        let request = match body {
            Some(value) => self.http.post(url).json(&value),
            None => self.http.get(url),
        };
        let mut response = request
            .bearer_auth(locator.token)
            .send()
            .await
            .map_err(|_| "daemon-unavailable")?;
        let status = response.status();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "management-response-incomplete")?
        {
            if bytes.len() + chunk.len() > 4 * 1024 * 1024 {
                return Err("management-response-too-large".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value =
            serde_json::from_slice(&bytes).map_err(|_| "invalid-management-response")?;
        if !status.is_success() {
            return Err(value["error"]
                .as_str()
                .filter(|s| {
                    s.len() <= 100 && s.bytes().all(|c| c.is_ascii_lowercase() || c == b'-')
                })
                .unwrap_or("management-request-failed")
                .into());
        }
        Ok(value)
    }
}
