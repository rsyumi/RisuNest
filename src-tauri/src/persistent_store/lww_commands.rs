use super::{
    commands::with_store_mut,
    lww::{self, Header},
    PersistentStoreState, StoreError,
};
use risunest_sync_wire::stamp::DecimalU64;
use serde::Deserialize;
use tauri::State;
use crate::native_log::logged;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReadOutbox {
    #[serde(flatten)]
    header: Header,
    limit: DecimalU64,
    #[serde(default)]
    generating: Vec<lww::MessageLocator>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Replacement {
    #[serde(flatten)]
    header: Header,
    staging_id: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct NewDeviceReplacement {
    #[serde(flatten)]
    header: Header,
    staging_id: String,
    authorization_id: String,
}

#[tauri::command(async)]
pub(crate) fn pds_lww_replace_target_as_new_device(
    state: State<'_, PersistentStoreState>,
    request: NewDeviceReplacement,
) -> Result<lww::NewDeviceResult, StoreError> {
    logged("pds_lww_replace_target_as_new_device", with_store_mut(state, |store| {
        store.lww_replace_target_as_new_device(
            &request.header,
            &request.staging_id,
            &request.authorization_id,
        )
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_lww_read_outbox(
    state: State<'_, PersistentStoreState>,
    request: ReadOutbox,
) -> Result<lww::OutboxPage, StoreError> {
    logged("pds_lww_read_outbox", with_store_mut(state, |store| {
        store.lww_read_outbox_generating(
            request.header.binding_authority,
            usize::try_from(request.limit.0).map_err(lww::error)?,
            &request.generating,
        )
    }))
}
#[tauri::command(async)]
pub(crate) fn pds_lww_stage_receive(
    state: State<'_, PersistentStoreState>,
    request: lww::StageReceive,
) -> Result<(), StoreError> {
    logged("pds_lww_stage_receive", with_store_mut(state, |store| store.lww_stage_receive(&request)))
}
#[tauri::command(async)]
pub(crate) fn pds_lww_apply_receive(
    state: State<'_, PersistentStoreState>,
    request: lww::ApplyReceive,
) -> Result<lww::ApplyResult, StoreError> {
    logged("pds_lww_apply_receive", with_store_mut(state, |store| store.lww_apply_receive(&request)))
}
#[tauri::command(async)]
pub(crate) fn pds_lww_finish_receive(
    state: State<'_, PersistentStoreState>,
    request: Header,
) -> Result<(), StoreError> {
    logged("pds_lww_finish_receive", with_store_mut(state, |store| store.lww_finish_receive(&request)))
}
#[tauri::command(async)]
pub(crate) fn pds_lww_drain_deferred(
    state: State<'_, PersistentStoreState>,
    request: lww::ApplyReceive,
) -> Result<lww::ApplyResult, StoreError> {
    logged("pds_lww_drain_deferred", with_store_mut(state, |store| store.lww_drain_deferred(&request)))
}
#[tauri::command(async)]
pub(crate) fn pds_lww_commit_replacement(
    state: State<'_, PersistentStoreState>,
    request: Replacement,
) -> Result<super::RevisionResult, StoreError> {
    logged("pds_lww_commit_replacement", with_store_mut(state, |store| {
        store.lww_commit_replacement(&request.header, &request.staging_id)
    }))
}

#[tauri::command(async)]
pub(crate) fn pds_lww_finish_initial_publication(
    state: State<'_, PersistentStoreState>,
    request: Header,
) -> Result<bool, StoreError> {
    logged("pds_lww_finish_initial_publication", with_store_mut(state, |store| {
        store.lww_finish_initial_publication(&request)
    }))
}

#[cfg(test)]
mod tests {
    use super::super::PersistentStore;
    use super::*;
    use serde_json::{json, Value};
    use std::{sync::mpsc, thread, time::Duration};
    use tauri::{ipc::{CallbackFn, InvokeBody, InvokeResponse}, webview::InvokeRequest, Manager};

    fn request(cmd: &str, body: Value) -> InvokeRequest {
        InvokeRequest {
            cmd: cmd.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            // The app origin differs by platform, and the mock runtime refuses any other origin at once.
            url: if cfg!(any(windows, target_os = "android")) { "http://tauri.localhost" } else { "tauri://localhost" }.parse().unwrap(),
            body: InvokeBody::Json(body),
            headers: Default::default(),
            invoke_key: tauri::test::INVOKE_KEY.to_string(),
        }
    }

    /// The renderer's bridge thread must not wait while a long operation holds
    /// the store, and the reads answer as before once it is free.
    #[test]
    fn binding_and_outbox_reads_leave_the_bridge_thread_while_the_store_is_held() {
        let directory = tempfile::tempdir().unwrap();
        let app = crate::test_app::MockApp::new(tauri::test::mock_builder()
            .invoke_handler(tauri::generate_handler![
                crate::persistent_store::sync_selection::pds_lww_binding_state,
                crate::persistent_store::sync_selection::pds_lww_binding_content,
                pds_lww_read_outbox,
            ])
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap());
        app.manage(PersistentStoreState::with_test_store(PersistentStore::open(directory.path()).unwrap()));
        let webview = tauri::WebviewWindowBuilder::new(&*app, "main", Default::default()).build().unwrap();
        let (state, content, authority) = with_store_mut(app.state(), |store| {
            Ok((store.lww_binding_state()?, store.lww_binding_content()?, store.lww_binding_authority()?))
        }).unwrap();
        let outbox_request = json!({"bindingAuthority": authority, "requestId": "synthetic-read", "limit": "10"});
        let outbox = with_store_mut(app.state(), |store| store.lww_read_outbox_generating(authority, 10, &[])).unwrap();
        let expected = [
            ("pds_lww_binding_state", json!({}), serde_json::to_value(state).unwrap()),
            ("pds_lww_binding_content", json!({}), serde_json::to_value(content).unwrap()),
            ("pds_lww_read_outbox", json!({"request": outbox_request}), serde_json::to_value(outbox).unwrap()),
        ];

        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let handle = app.handle().clone();
        let holder = thread::spawn(move || with_store_mut(handle.state(), |_| {
            held_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        }));
        held_rx.recv().unwrap();
        let (response_tx, response_rx) = mpsc::channel();
        let (dispatched_tx, dispatched_rx) = mpsc::channel();
        let requests: Vec<_> = expected.iter().map(|(cmd, body, _)| (cmd.to_string(), body.clone())).collect();
        let bridge = webview.clone();
        thread::spawn(move || {
            for (cmd, body) in requests {
                let response_tx = response_tx.clone();
                bridge.as_ref().clone().on_message(request(&cmd, body), Box::new(move |_, _, response, _, _| {
                    let _ = response_tx.send((cmd, response));
                }));
            }
            let _ = dispatched_tx.send(());
        });
        let dispatched = dispatched_rx.recv_timeout(Duration::from_secs(10));
        let early = response_rx.try_recv().is_ok();
        release_tx.send(()).unwrap();
        holder.join().unwrap().unwrap();
        assert!(dispatched.is_ok(), "a read waited on the bridge thread for the held store");
        assert!(!early, "a read answered while the store was held");

        for _ in 0..expected.len() {
            let (cmd, response) = response_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let body = match response {
                InvokeResponse::Ok(body) => body.deserialize::<Value>().unwrap(),
                InvokeResponse::Err(error) => panic!("{cmd} failed: {:?}", error.0),
            };
            let (_, _, want) = expected.iter().find(|(name, _, _)| *name == cmd).unwrap();
            assert_eq!(&body, want, "{cmd}");
        }
    }
}
