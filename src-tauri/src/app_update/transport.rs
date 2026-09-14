use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use risunest_release_update::{MetadataTransport, TransportError};
use url::Url;

#[derive(Clone)]
pub(crate) struct GithubTransport {
    client: reqwest::Client,
}

impl GithubTransport {
    pub(crate) fn new() -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(5))
            .user_agent("RisuNest updater")
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { client })
    }
}

#[async_trait]
impl MetadataTransport for GithubTransport {
    async fn get(
        &self,
        url: &Url,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<Vec<u8>, TransportError> {
        let response = self
            .client
            .get(url.clone())
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| TransportError::new(error.to_string()))?;
        if !response.status().is_success() {
            return Err(TransportError::new(format!(
                "GitHub returned HTTP {}",
                response.status()
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length > max_bytes as u64)
        {
            return Err(TransportError::new("response exceeds size limit"));
        }
        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| TransportError::new(error.to_string()))?;
            if bytes.len().saturating_add(chunk.len()) > max_bytes {
                return Err(TransportError::new("response exceeds size limit"));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }
}
