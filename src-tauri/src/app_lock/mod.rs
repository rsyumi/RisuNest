use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager, WebviewWindow};

#[cfg(target_os = "android")]
mod android;
#[cfg(target_os = "ios")]
mod ios;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;
#[cfg(desktop)]
mod desktop;

#[cfg(target_os = "android")]
use android as platform;
#[cfg(target_os = "ios")]
use ios as platform;
#[cfg(target_os = "macos")]
use macos as platform;
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(windows)]
use windows as platform;

pub(crate) const EVENT_NAME: &str = "risunest-app-lock";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AuthStatus { Unsupported, NotEnrolled, Available }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AuthMethod { WindowsHello, FaceId, TouchId, Device }

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthAvailability {
    pub status: AuthStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<AuthMethod>,
}

impl AuthAvailability {
    pub fn unsupported() -> Self { Self { status: AuthStatus::Unsupported, method: None } }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Capabilities {
    pub auth: AuthAvailability,
    pub preview_supported: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum AuthOutcome { Success, Cancelled, Unavailable, Failed }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum LockTiming {
    #[default]
    #[serde(rename = "launch")]
    Launch,
    #[serde(rename = "immediate")]
    Immediate,
    #[serde(rename = "after-1m")]
    After1m,
    #[serde(rename = "after-5m")]
    After5m,
    #[serde(rename = "after-15m")]
    After15m,
    #[serde(rename = "after-30m")]
    After30m,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CoverPolicy {
    pub enabled: bool,
    pub timing: LockTiming,
    pub gate_required: bool,
    pub hide_recents_preview: bool,
    pub background_color: String,
}

impl CoverPolicy {
    pub fn installs_return_cover(&self) -> bool {
        self.enabled && self.timing != LockTiming::Launch
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LifecycleSnapshot {
    pub active: bool,
    pub epoch: u64,
    pub at_ms: u64,
    pub departed_at_ms: Option<u64>,
}

struct SetupState(Result<(), String>);

pub(crate) fn setup(app: &AppHandle) {
    let result = platform::setup(app);
    if let Err(error) = &result { crate::nlog!("warn", "app-lock setup unavailable: {error}"); }
    app.manage(SetupState(result));
}

fn ensure_ready(app: &AppHandle) -> Result<(), String> {
    app.try_state::<SetupState>().ok_or("app-lock setup is unavailable")?.0.clone()
}

#[tauri::command]
pub(crate) async fn app_lock_check(app: AppHandle) -> Result<Capabilities, String> {
    ensure_ready(&app)?;
    platform::check(&app).await
}

#[tauri::command]
pub(crate) async fn app_lock_authenticate(window: WebviewWindow, reason: String) -> Result<AuthOutcome, String> {
    ensure_ready(window.app_handle())?;
    platform::authenticate(&window, reason).await
}

#[tauri::command]
pub(crate) async fn app_lock_set_policy(app: AppHandle, policy: CoverPolicy) -> Result<LifecycleSnapshot, String> {
    ensure_ready(&app)?;
    let result = platform::set_policy(&app, &policy).await;
    if let Err(error) = &result { crate::nlog!("warn", "app-lock policy update failed: {error}"); }
    result
}

// A frame confirmation waits for a platform presentation callback. One that has not answered by
// then counts as refused, so the page presents its frame again instead of waiting on it.
const FRAME_ANSWER_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

#[tauri::command]
pub(crate) async fn app_lock_ready(app: AppHandle, epoch: u64, locked: bool) -> Result<bool, String> {
    ensure_ready(&app)?;
    answer_frame(epoch, locked, platform::ready(&app, epoch, locked), FRAME_ANSWER_LIMIT).await
}

/// Each frame confirmation leaves one line in the device log with the time its platform answer took.
async fn answer_frame(epoch: u64, locked: bool, answer: impl std::future::Future<Output = Result<bool, String>>, limit: std::time::Duration) -> Result<bool, String> {
    let started = std::time::Instant::now();
    let answer = tokio::time::timeout(limit, answer).await;
    let elapsed = started.elapsed().as_millis();
    match &answer {
        Ok(Ok(accepted)) => crate::nlog!("info", "app-lock frame {}: epoch {epoch}, locked {locked}, {elapsed} ms",
            if *accepted { "accepted" } else { "refused" }),
        Ok(Err(error)) => crate::nlog!("warn", "app-lock frame confirmation failed: epoch {epoch}, locked {locked}, {elapsed} ms: {error}"),
        Err(_) => crate::nlog!("warn", "app-lock frame unanswered: epoch {epoch}, locked {locked}, {elapsed} ms"),
    }
    answer.unwrap_or(Ok(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn every_frame_answer_is_logged_and_an_unanswered_frame_counts_as_refused() {
        let limit = std::time::Duration::from_millis(5);
        assert_eq!(answer_frame(71_001, true, async { Ok(true) }, limit).await, Ok(true));
        assert_eq!(answer_frame(71_002, false, async { Ok(false) }, limit).await, Ok(false));
        assert_eq!(answer_frame(71_003, false, async { Err("synthetic".to_owned()) }, limit).await, Err("synthetic".to_owned()));
        assert_eq!(answer_frame(71_004, true, std::future::pending(), limit).await, Ok(false));
        let entries = crate::native_log::global_state().tail(None);
        let line = |epoch: u64| entries.iter().rev().find(|entry| entry.message.contains(&format!("epoch {epoch},"))).map(|entry| (entry.level.as_str(), entry.message.as_str()));
        assert!(matches!(line(71_001), Some(("info", message)) if message.starts_with("app-lock frame accepted: epoch 71001, locked true")));
        assert!(matches!(line(71_002), Some(("info", message)) if message.starts_with("app-lock frame refused: epoch 71002, locked false")));
        assert!(matches!(line(71_003), Some(("warn", message)) if message.starts_with("app-lock frame confirmation failed: epoch 71003") && message.ends_with(": synthetic")));
        assert!(matches!(line(71_004), Some(("warn", message)) if message.starts_with("app-lock frame unanswered: epoch 71004, locked true")));
    }
}
