use crate::{client::Client, Result};
use fs2::FileExt;
use risunest_sync_server::management::discovery::Discovery;
use serde_json::json;
use std::{fs::OpenOptions, path::Path, time::Duration};

fn same_locator(left: &Discovery, right: &Discovery) -> bool {
    left.address == right.address && left.token == right.token
}
pub fn locator_exists(root: &Path) -> Result<bool> {
    root.join("management-session")
        .try_exists()
        .map_err(|_| "management-session-unavailable".into())
}

pub async fn wait_stopped(root: &Path, locator: &Discovery, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !locator_exists(root)? {
            if owner_released(root)? {
                return Ok(());
            }
        } else {
            match Discovery::load(root) {
                Ok(current) if !same_locator(locator, &current) => {
                    return Err("management-session-changed".into())
                }
                Err(error) => {
                    if locator_exists(root)? {
                        return Err(error.code.into());
                    }
                    if owner_released(root)? {
                        return Ok(());
                    }
                }
                _ => {}
            }
            if owner_released(root)? {
                return Ok(());
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("server-stop-timeout".into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn owner_released(root: &Path) -> Result<bool> {
    let path = root.join("owner.lock");
    if !path.exists() {
        return Ok(true);
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|_| "server-owner-lock-unavailable".to_owned())?;
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

pub fn owner_active(root: &Path) -> Result<bool> {
    owner_released(root).map(|released| !released)
}

pub async fn wait_owner_released(root: &Path, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if owner_released(root)? {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("server-stop-timeout".into());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Stop only the daemon authenticated by this data directory's private locator.
pub async fn stop(root: &Path, client: &Client) -> Result<()> {
    if !locator_exists(root)? {
        return wait_owner_released(root, Duration::from_secs(30)).await;
    }
    let locator = Discovery::load(root).map_err(|error| error.code.to_owned())?;
    let state = match client.status_with_locator(&locator).await {
        Ok(state) => state,
        Err(error) if error == "daemon-unavailable" => {
            tokio::time::sleep(Duration::from_millis(100)).await;
            match client.status_with_locator(&locator).await {
                Ok(state) => state,
                Err(error) if error == "daemon-unavailable" => match Discovery::load(root) {
                    Ok(current) if same_locator(&locator, &current) => {
                        return wait_stopped(root, &locator, Duration::from_secs(30)).await
                    }
                    Ok(_) => return Err("management-session-changed".into()),
                    Err(error) => {
                        if locator_exists(root)? {
                            return Err(error.code.into());
                        }
                        return wait_owner_released(root, Duration::from_secs(30)).await;
                    }
                },
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    client
        .mutate_with_locator("shutdown", json!({"revision":state["revision"]}), &locator)
        .await?;
    wait_stopped(root, &locator, Duration::from_secs(30)).await
}
