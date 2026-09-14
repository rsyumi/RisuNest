use crate::{client::Client, Result};
use risunest_sync_server::management::discovery::Discovery;
use serde_json::json;
use std::{path::Path, time::Duration};

fn same_locator(left: &Discovery, right: &Discovery) -> bool {
    left.address == right.address && left.token == right.token
}
fn locator_exists(root: &Path) -> Result<bool> {
    root.join("management-session")
        .try_exists()
        .map_err(|_| "management-session-unavailable".into())
}

/// Stop only the daemon authenticated by this data directory's private locator.
pub async fn stop(root: &Path, client: &Client) -> Result<()> {
    if !locator_exists(root)? {
        return Ok(());
    }
    let locator = Discovery::load(root).map_err(|error| error.code.to_owned())?;
    let state = match client.status_with_locator(&locator).await {
        Ok(state) => state,
        Err(error) if error == "daemon-unavailable" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            match client.status_with_locator(&locator).await {
                Ok(state) => state,
                Err(error) if error == "daemon-unavailable" => {
                    match Discovery::load(root) {
                        Ok(current) if same_locator(&locator, &current) => return Ok(()),
                        Ok(_) => return Err("management-session-changed".into()),
                        Err(error) => {
                            if locator_exists(root)? {
                                return Err(error.code.into());
                            }
                            return Ok(());
                        }
                    }
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    client
        .mutate_with_locator(
            "shutdown",
            json!({"revision":state["revision"]}),
            &locator,
        )
        .await?;
    for _ in 0..150 {
        if !locator_exists(root)? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err("server-stop-timeout".into())
}
