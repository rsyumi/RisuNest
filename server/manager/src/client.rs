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
    pub(crate) async fn status_with_locator(&self, locator: &Discovery) -> Result<Value> {
        self.send_with_locator("status", None, locator).await
    }
    pub async fn mutate(&self, path: &str, body: Value) -> Result<Value> {
        let locator = Discovery::load(&self.root).map_err(|e| e.code.to_owned())?;
        self.mutate_with_locator(path, body, &locator).await
    }
    pub(crate) async fn mutate_with_locator(
        &self,
        path: &str,
        body: Value,
        locator: &Discovery,
    ) -> Result<Value> {
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
        self.send_with_locator(path, Some(body), locator).await
    }
    async fn send(&self, path: &str, body: Option<Value>) -> Result<Value> {
        // Reload only for each new user operation; never retry a mutation automatically.
        let locator = Discovery::load(&self.root).map_err(|e| e.code.to_owned())?;
        self.send_with_locator(path, body, &locator).await
    }
    async fn send_with_locator(
        &self,
        path: &str,
        body: Option<Value>,
        locator: &Discovery,
    ) -> Result<Value> {
        let url = format!("http://{}/{path}", locator.address);
        let request = match body {
            Some(value) => self.http.post(url).json(&value),
            None => self.http.get(url),
        };
        let mut response = request
            .bearer_auth(&locator.token)
            .send()
            .await
            .map_err(|error| {
                if error.is_connect() {
                    "daemon-unavailable"
                } else if error.is_timeout() {
                    "management-response-timeout"
                } else {
                    "management-request-failed"
                }
            })?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    #[tokio::test]
    async fn captured_locator_is_used_after_the_discovery_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let locator = Discovery {
            address: listener.local_addr().unwrap(),
            token: "a".repeat(64),
        };
        let server = std::thread::spawn(move || {
            for (method, response) in [
                ("GET /status ", r#"{"revision":"synthetic:0"}"#),
                ("POST /shutdown ", r#"{"stopping":true}"#),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 4096];
                let count = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..count]);
                assert!(request.starts_with(method));
                assert!(request.to_ascii_lowercase().contains(&format!(
                    "authorization: bearer {}",
                    "a".repeat(64)
                )));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.len(),
                    response
                )
                .unwrap();
            }
        });
        let client = Client::new(temp.path().to_owned()).unwrap();
        let status = client.status_with_locator(&locator).await.unwrap();
        std::fs::write(temp.path().join("management-session"), b"replacement").unwrap();

        client
            .mutate_with_locator(
                "shutdown",
                json!({"revision":status["revision"]}),
                &locator,
            )
            .await
            .unwrap();
        server.join().unwrap();
    }
}
