//! Change notifications for the synchronisation scheduler. The device
//! credential stays in this process: the renderer is only told that something
//! may have moved and confirms the remote head itself.
use tauri::{AppHandle, Emitter, Runtime};

/// Emitted after a local write advanced the device revision in a section that
/// can be synchronised.
pub(crate) const DEVICE_CHANGED_EVENT: &str = "risu-server-sync-device-changed";

/// Emitted when the remote may have moved. Carries no payload at all, so the
/// renderer cannot mistake a notification for a confirmed head.
pub(crate) const REMOTE_HINT_EVENT: &str = "risu-server-sync-remote-hint";

pub(crate) fn notify_device_changed<R: Runtime>(app: &AppHandle<R>) {
    // A lost notification costs latency; the scheduler still polls.
    let _ = app.emit(DEVICE_CHANGED_EVENT, ());
}

pub(crate) fn notify_remote_hint<R: Runtime>(app: &AppHandle<R>) {
    let _ = app.emit(REMOTE_HINT_EVENT, ());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tauri::Listener;

    /// Invariant 22 at the notification boundary. The renderer is told that
    /// something moved and nothing else: no endpoint, device or token.
    #[test]
    fn a_change_notification_reaches_the_renderer_carrying_no_credential() {
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .expect("build a renderer host");
        let seen = Arc::new(Mutex::new(Vec::new()));
        for name in [DEVICE_CHANGED_EVENT, REMOTE_HINT_EVENT] {
            let seen = seen.clone();
            app.listen(name, move |event| {
                seen.lock()
                    .unwrap()
                    .push((name, event.payload().to_owned()));
            });
        }
        notify_device_changed(app.handle());
        notify_remote_hint(app.handle());
        assert_eq!(
            seen.lock().unwrap().clone(),
            vec![
                (DEVICE_CHANGED_EVENT, "null".to_owned()),
                (REMOTE_HINT_EVENT, "null".to_owned()),
            ]
        );
    }
}
