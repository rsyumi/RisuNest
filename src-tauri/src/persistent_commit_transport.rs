//! Windows-only transport for large commits. Store/revision semantics stay in persistent_store.
use crate::persistent_commit_raw::commit_bytes;
use crate::persistent_store::{RevisionResult, StoreError, StoreResult};
use serde::Serialize;
use std::cell::RefCell;
use tauri::{AppHandle, WebviewWindow};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2Environment12, ICoreWebView2SharedBuffer, ICoreWebView2_17,
    COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_WRITE,
};
use windows::{
    core::{Interface, PCWSTR},
    Win32::System::Com::STREAM_SEEK_SET,
};

const CAPACITY: usize = 8 * 1024 * 1024;
const MAX_BYTES: usize = 64 * 1024 * 1024;
fn invalid(message: impl ToString) -> StoreError {
    StoreError::Validation {
        message: message.to_string(),
    }
}
fn native_error(error: impl std::fmt::Display) -> StoreError {
    StoreError::Store {
        message: error.to_string(),
    }
}
fn guard(window: &WebviewWindow) -> StoreResult<()> {
    if window.label() != "main" {
        return Err(invalid("commit transport requires the main webview"));
    }
    Ok(())
}

struct Transfer {
    id: String,
    request_id: String,
    total: usize,
    bytes: Vec<u8>,
}
impl Transfer {
    fn new(request_id: String, total: usize) -> StoreResult<Self> {
        if total == 0 || total > MAX_BYTES {
            return Err(invalid("invalid shared commit size"));
        }
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(total).map_err(native_error)?;
        Ok(Self {
            id: uuid::Uuid::new_v4().to_string(),
            request_id,
            total,
            bytes,
        })
    }
    fn validate_chunk(&self, id: &str, offset: usize, length: usize) -> StoreResult<()> {
        if self.id != id
            || offset != self.bytes.len()
            || length == 0
            || length > CAPACITY
            || length > self.total - self.bytes.len()
        {
            return Err(invalid("invalid shared commit chunk"));
        }
        Ok(())
    }
    fn ready(&self, id: &str) -> StoreResult<()> {
        if self.id != id || self.bytes.len() != self.total {
            return Err(invalid("incomplete or stale shared commit"));
        }
        Ok(())
    }
}
struct Pool {
    buffer: ICoreWebView2SharedBuffer,
    transfer: Option<Transfer>,
}

fn cancel_transfer(transfer: &mut Option<Transfer>, request_id: &str) {
    if transfer
        .as_ref()
        .is_some_and(|transfer| transfer.request_id == request_id)
    {
        *transfer = None;
    }
}

impl Pool {
    fn cancel_request(&mut self, request_id: &str) {
        cancel_transfer(&mut self.transfer, request_id);
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        unsafe {
            let _ = self.buffer.Close();
        }
    }
}
thread_local! { static POOL: RefCell<Option<Pool>> = const { RefCell::new(None) }; }

#[derive(Serialize)]
pub(crate) struct Opened {
    id: String,
    capacity: usize,
}

#[tauri::command]
pub(crate) async fn pds_commit_shared_open(
    window: WebviewWindow,
    request_id: String,
    total_bytes: usize,
) -> StoreResult<Option<Opened>> {
    guard(&window)?;
    if uuid::Uuid::parse_str(&request_id).is_err() {
        return Err(invalid("invalid shared commit request ID"));
    }
    if total_bytes == 0 || total_bytes > MAX_BYTES {
        return Err(invalid("invalid shared commit size"));
    }
    let (sender, receiver) = futures::channel::oneshot::channel();
    window.with_webview(move |webview| {
        let result = POOL.with(|slot| -> StoreResult<Option<Opened>> {
            let mut pool = slot.borrow_mut();
            if pool.as_ref().is_some_and(|pool| pool.transfer.is_some()) { return Err(invalid("shared commit already active")); }
            unsafe {
                let environment = match webview.environment().cast::<ICoreWebView2Environment12>() {
                    Ok(value) => value,
                    Err(error) if error.code().0 == 0x80004002_u32 as i32 => return Ok(None),
                    Err(error) => return Err(native_error(error)),
                };
                let view = match webview.controller().CoreWebView2().map_err(native_error)?.cast::<ICoreWebView2_17>() {
                    Ok(value) => value,
                    Err(error) if error.code().0 == 0x80004002_u32 as i32 => return Ok(None),
                    Err(error) => return Err(native_error(error)),
                };
                if pool.is_none() { *pool = Some(Pool { buffer: environment.CreateSharedBuffer(CAPACITY as u64).map_err(native_error)?, transfer: None }); }
                let pool = pool.as_mut().unwrap();
                // Reserve payload memory only after acquiring the single producer slot.
                let transfer = Transfer::new(request_id.clone(), total_bytes)?;
                let metadata = serde_json::json!({ "kind": "pds-commit", "requestId": request_id, "id": transfer.id }).to_string();
                let metadata: Vec<u16> = metadata.encode_utf16().chain(Some(0)).collect();
                view.PostSharedBufferToScript(&pool.buffer, COREWEBVIEW2_SHARED_BUFFER_ACCESS_READ_WRITE, PCWSTR(metadata.as_ptr())).map_err(native_error)?;
                let opened = Opened { id: transfer.id.clone(), capacity: CAPACITY };
                pool.transfer = Some(transfer);
                Ok(Some(opened))
            }
        });
        let _ = sender.send(result);
    }).map_err(native_error)?;
    receiver.await.map_err(native_error)?
}

#[tauri::command]
pub(crate) async fn pds_commit_shared_chunk(
    window: WebviewWindow,
    id: String,
    offset: usize,
    length: usize,
) -> StoreResult<usize> {
    guard(&window)?;
    let (sender, receiver) = futures::channel::oneshot::channel();
    window
        .with_webview(move |_| {
            let result = POOL.with(|slot| -> StoreResult<usize> {
                let mut borrow = slot.borrow_mut();
                let pool = borrow
                    .as_mut()
                    .ok_or_else(|| invalid("no shared commit buffer"))?;
                let transfer = pool
                    .transfer
                    .as_mut()
                    .ok_or_else(|| invalid("no active shared commit"))?;
                transfer.validate_chunk(&id, offset, length)?;
                let mut bytes = vec![0_u8; length];
                // Read through the COM stream into Rust-owned bytes. Do not create a Rust
                // reference into memory that the renderer can mutate independently.
                unsafe {
                    let stream = pool.buffer.OpenStream().map_err(native_error)?;
                    stream
                        .Seek(0, STREAM_SEEK_SET, None)
                        .map_err(native_error)?;
                    let mut read = 0;
                    stream
                        .Read(bytes.as_mut_ptr().cast(), length as u32, Some(&mut read))
                        .ok()
                        .map_err(native_error)?;
                    if read as usize != length {
                        return Err(invalid("short shared commit read"));
                    }
                }
                transfer.bytes.extend_from_slice(&bytes);
                Ok(transfer.bytes.len())
            });
            let _ = sender.send(result);
        })
        .map_err(native_error)?;
    receiver.await.map_err(native_error)?
}

#[tauri::command]
pub(crate) async fn pds_commit_shared_finish(
    app: AppHandle,
    window: WebviewWindow,
    id: String,
) -> StoreResult<RevisionResult> {
    guard(&window)?;
    let (sender, receiver) = futures::channel::oneshot::channel();
    window
        .with_webview(move |_| {
            let result = POOL.with(|slot| -> StoreResult<Vec<u8>> {
                let mut borrow = slot.borrow_mut();
                let pool = borrow
                    .as_mut()
                    .ok_or_else(|| invalid("no shared commit buffer"))?;
                pool.transfer
                    .as_ref()
                    .ok_or_else(|| invalid("no active shared commit"))?
                    .ready(&id)?;
                Ok(pool.transfer.take().unwrap().bytes)
            });
            let _ = sender.send(result);
        })
        .map_err(native_error)?;
    let bytes = receiver.await.map_err(native_error)??;
    tauri::async_runtime::spawn_blocking(move || commit_bytes(&app, &bytes))
        .await
        .map_err(native_error)?
}

#[tauri::command]
pub(crate) async fn pds_commit_shared_cancel(
    window: WebviewWindow,
    request_id: String,
) -> StoreResult<()> {
    guard(&window)?;
    let (sender, receiver) = futures::channel::oneshot::channel();
    window
        .with_webview(move |_| {
            POOL.with(|slot| {
                if let Some(pool) = slot.borrow_mut().as_mut() {
                    pool.cancel_request(&request_id);
                }
            });
            let _ = sender.send(());
        })
        .map_err(native_error)?;
    receiver.await.map_err(native_error)
}

/// Called on main-page navigation so an interrupted producer cannot retain a lease.
pub(crate) fn reset() {
    POOL.with(|slot| {
        slot.borrow_mut().take();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_invalid_ranges_and_requires_complete_ordered_payload() {
        assert!(Transfer::new("request".to_owned(), 0).is_err());
        assert!(Transfer::new("request".to_owned(), MAX_BYTES + 1).is_err());
        let mut transfer = Transfer::new("request".to_owned(), 10).unwrap();
        let id = transfer.id.clone();
        for (token, offset, length) in [
            ("stale", 0, 1),
            (&*id, 1, 1),
            (&*id, 0, 0),
            (&*id, 0, 11),
            (&*id, usize::MAX, 1),
        ] {
            assert!(transfer.validate_chunk(token, offset, length).is_err());
        }
        assert!(transfer.ready(&id).is_err());
        transfer.validate_chunk(&id, 0, 5).unwrap();
        transfer.bytes.extend_from_slice(&[1; 5]);
        assert!(transfer.validate_chunk(&id, 0, 5).is_err());
        transfer.validate_chunk(&id, 5, 5).unwrap();
        transfer.bytes.extend_from_slice(&[2; 5]);
        transfer.ready(&id).unwrap();
        assert!(transfer.ready("stale").is_err());
        assert!(transfer.validate_chunk(&id, 10, 1).is_err());
    }

    #[test]
    fn request_cleanup_cancels_only_the_matching_open_attempt() {
        let first_request = uuid::Uuid::new_v4().to_string();
        let mut active = Some(Transfer::new(first_request.clone(), 10).unwrap());
        let internal_id = active.as_ref().unwrap().id.clone();

        cancel_transfer(&mut active, "unknown-request");
        assert!(active.is_some());
        cancel_transfer(&mut active, &internal_id);
        assert!(active.is_some());
        cancel_transfer(&mut active, &first_request);
        assert!(active.is_none());

        let second_request = uuid::Uuid::new_v4().to_string();
        active = Some(Transfer::new(second_request, 10).unwrap());
        cancel_transfer(&mut active, &first_request);
        assert!(active.is_some());
    }
}
