use crate::{client::Client, Result};
use serde_json::json;
use std::{path::Path, time::Duration};

/// Stop only the daemon authenticated by this data directory's private locator.
pub async fn stop(root: &Path, client: &Client) -> Result<()> {
    if !root.join("management-session").exists() {
        return Ok(());
    }
    let state = client.status().await?;
    client
        .mutate("shutdown", json!({"revision":state["revision"]}))
        .await?;
    for _ in 0..150 {
        if !root.join("management-session").exists() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err("server-stop-timeout".into())
}
