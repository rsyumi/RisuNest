use super::{
    encode_inlay_image, write_inlay_image_with_options, EncodedInlayImage, InlayEncodeOptions,
    InlayImageMetadata,
};
use crate::persistent_store::PersistentStoreState;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
};
use tauri::{AppHandle, Manager};

pub(crate) const NATIVE_MEDIA_IPC_CHUNK_BYTES: usize = 64 * 1024;
const MAX_NATIVE_MEDIA_TRANSFER_BYTES: u64 = 512 * 1024 * 1024;
const MAX_CONCURRENT_NATIVE_MEDIA_TRANSFERS: usize = 4;
const MAX_TOTAL_NATIVE_MEDIA_TRANSFER_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(serde::Serialize)]
pub(crate) struct NativeMediaInputOpened {
    capacity: usize,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EncodedInlayIpcResult {
    data: Option<Vec<u8>>,
    output_id: Option<String>,
    output_size: u64,
    metadata: InlayImageMetadata,
}

struct Uploading {
    generation: u64,
    total_bytes: u64,
    received: u64,
    file: tempfile::NamedTempFile,
}

struct Processing {
    generation: u64,
    reserved_bytes: u64,
    cancelled: bool,
}

struct Download {
    generation: u64,
    total_bytes: u64,
    sent: u64,
    file: tempfile::NamedTempFile,
}

enum Transfer {
    Uploading(Uploading),
    Processing(Processing),
    Download(Download),
}

impl Transfer {
    fn reserved_bytes(&self) -> u64 {
        match self {
            Self::Uploading(upload) => upload.total_bytes,
            Self::Processing(processing) => processing.reserved_bytes,
            Self::Download(download) => download.total_bytes,
        }
    }
}

#[derive(Default)]
struct TransferPool {
    generation: u64,
    transfers: HashMap<String, Transfer>,
    reserved_bytes: u64,
}

struct ProcessingInput {
    id: String,
    generation: u64,
    total_bytes: u64,
    file: tempfile::NamedTempFile,
}

impl TransferPool {
    fn reserve(&self, bytes: u64) -> Result<u64, String> {
        if bytes > MAX_NATIVE_MEDIA_TRANSFER_BYTES {
            return Err("native media transfer exceeds the maximum size".to_owned());
        }
        self.reserved_bytes
            .checked_add(bytes)
            .filter(|total| *total <= MAX_TOTAL_NATIVE_MEDIA_TRANSFER_BYTES)
            .ok_or_else(|| "native media transfer quota is unavailable".to_owned())
    }

    fn open(
        &mut self,
        id: String,
        total_bytes: u64,
        file: tempfile::NamedTempFile,
    ) -> Result<(), String> {
        if uuid::Uuid::parse_str(&id).is_err() {
            return Err("invalid native media transfer ID".to_owned());
        }
        if total_bytes <= NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 {
            return Err("native media streamed input must exceed one IPC chunk".to_owned());
        }
        if self.transfers.len() >= MAX_CONCURRENT_NATIVE_MEDIA_TRANSFERS
            || self.transfers.contains_key(&id)
        {
            return Err("native media transfer capacity is unavailable".to_owned());
        }
        let reserved_bytes = self.reserve(total_bytes)?;
        self.transfers.insert(
            id,
            Transfer::Uploading(Uploading {
                generation: self.generation,
                total_bytes,
                received: 0,
                file,
            }),
        );
        self.reserved_bytes = reserved_bytes;
        Ok(())
    }

    fn append(&mut self, id: &str, offset: u64, chunk: &[u8]) -> Result<u64, String> {
        let Some(Transfer::Uploading(upload)) = self.transfers.get(id) else {
            return Err("native media input upload is unavailable".to_owned());
        };
        if offset != upload.received
            || chunk.is_empty()
            || chunk.len() > NATIVE_MEDIA_IPC_CHUNK_BYTES
        {
            return Err("invalid native media input chunk".to_owned());
        }
        let next = upload
            .received
            .checked_add(chunk.len() as u64)
            .filter(|next| *next <= upload.total_bytes)
            .ok_or_else(|| "native media input exceeds its declared length".to_owned())?;
        let write_result = match self.transfers.get_mut(id) {
            Some(Transfer::Uploading(upload)) => upload.file.write_all(chunk),
            _ => unreachable!("validated native media upload disappeared"),
        };
        if let Err(error) = write_result {
            self.cancel(id);
            return Err(format!("failed to append native media input: {error}"));
        }
        match self.transfers.get_mut(id) {
            Some(Transfer::Uploading(upload)) => upload.received = next,
            _ => unreachable!("written native media upload disappeared"),
        }
        Ok(next)
    }

    fn begin_uploaded_processing(&mut self, id: &str) -> Result<ProcessingInput, String> {
        if !matches!(self.transfers.get(id), Some(Transfer::Uploading(_))) {
            return Err("native media input upload is not ready to finish".to_owned());
        }
        let transfer = self
            .transfers
            .remove(id)
            .ok_or_else(|| "native media input upload is unavailable".to_owned())?;
        let Transfer::Uploading(mut upload) = transfer else {
            unreachable!()
        };
        if upload.received != upload.total_bytes {
            self.reserved_bytes -= upload.total_bytes;
            return Err("native media input upload is incomplete".to_owned());
        }
        if let Err(error) = upload.file.flush() {
            self.reserved_bytes -= upload.total_bytes;
            return Err(format!("failed to flush native media input: {error}"));
        }
        let generation = upload.generation;
        self.transfers.insert(
            id.to_owned(),
            Transfer::Processing(Processing {
                generation,
                reserved_bytes: upload.total_bytes,
                cancelled: false,
            }),
        );
        Ok(ProcessingInput {
            id: id.to_owned(),
            generation,
            total_bytes: upload.total_bytes,
            file: upload.file,
        })
    }

    fn begin_direct_processing(&mut self, reserved_bytes: u64) -> Result<(String, u64), String> {
        if reserved_bytes > NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 {
            return Err("native media direct input exceeds one IPC chunk".to_owned());
        }
        if self.transfers.len() >= MAX_CONCURRENT_NATIVE_MEDIA_TRANSFERS {
            return Err("native media transfer capacity is unavailable".to_owned());
        }
        let total = self.reserve(reserved_bytes)?;
        let id = uuid::Uuid::new_v4().to_string();
        let generation = self.generation;
        self.transfers.insert(
            id.clone(),
            Transfer::Processing(Processing {
                generation,
                reserved_bytes,
                cancelled: false,
            }),
        );
        self.reserved_bytes = total;
        Ok((id, generation))
    }

    fn complete_inline(&mut self, id: &str, generation: u64) -> Result<(), String> {
        let Some(Transfer::Processing(processing)) = self.transfers.get(id) else {
            return Err("native media processing was cancelled".to_owned());
        };
        if processing.cancelled
            || processing.generation != generation
            || generation != self.generation
        {
            self.finish_processing(id);
            return Err("native media renderer session changed during processing".to_owned());
        }
        self.finish_processing(id);
        Ok(())
    }

    fn reserve_encoded_output(
        &mut self,
        id: &str,
        generation: u64,
        total_bytes: u64,
    ) -> Result<(), String> {
        let Some(Transfer::Processing(processing)) = self.transfers.get(id) else {
            return Err("native media processing was cancelled".to_owned());
        };
        if processing.cancelled
            || processing.generation != generation
            || generation != self.generation
        {
            return Err("native media renderer session changed during processing".to_owned());
        }
        if total_bytes > MAX_NATIVE_MEDIA_TRANSFER_BYTES {
            return Err("encoded native media exceeds the maximum transfer size".to_owned());
        }
        let next_reserved = self
            .reserved_bytes
            .checked_sub(processing.reserved_bytes)
            .and_then(|reserved| reserved.checked_add(total_bytes))
            .filter(|reserved| *reserved <= MAX_TOTAL_NATIVE_MEDIA_TRANSFER_BYTES)
            .ok_or_else(|| "native media output quota is unavailable".to_owned())?;
        let Some(Transfer::Processing(processing)) = self.transfers.get_mut(id) else {
            unreachable!("validated native media processing disappeared")
        };
        processing.reserved_bytes = total_bytes;
        self.reserved_bytes = next_reserved;
        Ok(())
    }

    fn complete_download(
        &mut self,
        processing_id: &str,
        download_id: String,
        generation: u64,
        total_bytes: u64,
        file: tempfile::NamedTempFile,
    ) -> Result<(), String> {
        let Some(Transfer::Processing(processing)) = self.transfers.get(processing_id) else {
            return Err("native media processing was cancelled".to_owned());
        };
        let processing_generation = processing.generation;
        let reserved_bytes = processing.reserved_bytes;
        let cancelled = processing.cancelled;
        if cancelled || processing_generation != generation || generation != self.generation {
            self.finish_processing(processing_id);
            return Err("native media renderer session changed during processing".to_owned());
        }
        if uuid::Uuid::parse_str(&download_id).is_err()
            || (download_id != processing_id && self.transfers.contains_key(&download_id))
        {
            self.finish_processing(processing_id);
            return Err("invalid native media output transfer ID".to_owned());
        }
        if total_bytes > MAX_NATIVE_MEDIA_TRANSFER_BYTES {
            self.finish_processing(processing_id);
            return Err("encoded native media exceeds the maximum transfer size".to_owned());
        }
        if reserved_bytes != total_bytes {
            self.finish_processing(processing_id);
            return Err("native media output reservation changed".to_owned());
        }
        self.transfers.remove(processing_id);
        self.transfers.insert(
            download_id,
            Transfer::Download(Download {
                generation,
                total_bytes,
                sent: 0,
                file,
            }),
        );
        Ok(())
    }

    fn read_download(
        &mut self,
        id: &str,
        start: u64,
        end_exclusive: u64,
    ) -> Result<Vec<u8>, String> {
        let length = end_exclusive
            .checked_sub(start)
            .ok_or_else(|| "invalid native media output range".to_owned())?;
        let read_result = (|| {
            let Some(Transfer::Download(download)) = self.transfers.get_mut(id) else {
                return Err("native media output is unavailable".to_owned());
            };
            if download.generation != self.generation
                || start != download.sent
                || length == 0
                || length > NATIVE_MEDIA_IPC_CHUNK_BYTES as u64
                || end_exclusive > download.total_bytes
            {
                return Err("invalid native media output range".to_owned());
            }
            download
                .file
                .seek(SeekFrom::Start(start))
                .map_err(|error| format!("failed to seek native media output: {error}"))?;
            let mut bytes = vec![0; length as usize];
            download
                .file
                .read_exact(&mut bytes)
                .map_err(|error| format!("failed to read native media output: {error}"))?;
            download.sent = end_exclusive;
            Ok(bytes)
        })();
        if read_result.is_err() && matches!(self.transfers.get(id), Some(Transfer::Download(_))) {
            self.cancel(id);
        }
        read_result
    }

    fn cancel(&mut self, id: &str) {
        if let Some(transfer) = self.transfers.remove(id) {
            self.reserved_bytes -= transfer.reserved_bytes();
        }
    }

    fn cancel_renderer(&mut self, id: &str) {
        if let Some(Transfer::Processing(processing)) = self.transfers.get_mut(id) {
            processing.cancelled = true;
            return;
        }
        self.cancel(id);
    }

    fn finish_processing(&mut self, id: &str) {
        if matches!(self.transfers.get(id), Some(Transfer::Processing(_))) {
            self.cancel(id);
        }
    }

    fn reset_renderer_session(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        let removable = self
            .transfers
            .iter_mut()
            .filter_map(|(id, transfer)| match transfer {
                Transfer::Processing(processing) => {
                    processing.cancelled = true;
                    None
                }
                _ => Some((id.clone(), transfer.reserved_bytes())),
            })
            .collect::<Vec<_>>();
        for (id, reserved_bytes) in removable {
            self.transfers.remove(&id);
            self.reserved_bytes -= reserved_bytes;
        }
    }
}

#[derive(Default)]
pub(crate) struct NativeMediaIpcState {
    pool: Mutex<TransferPool>,
    spool_root: Mutex<Option<PathBuf>>,
    processing: Mutex<()>,
}

impl NativeMediaIpcState {
    fn admit_processing(&self, id: &str) -> Result<MutexGuard<'_, ()>, String> {
        let result = (|| {
            let guard = self.processing.lock()
                .map_err(|error| format!("native media processing mutex poisoned: {error}"))?;
            let pool = self.pool.lock()
                .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?;
            if !matches!(pool.transfers.get(id), Some(Transfer::Processing(processing))
                if !processing.cancelled && processing.generation == pool.generation)
            {
                return Err("native media processing was cancelled".to_owned());
            }
            Ok(guard)
        })();
        if result.is_err() {
            if let Ok(mut pool) = self.pool.lock() {
                pool.finish_processing(id);
            }
        }
        result
    }

    pub(crate) fn configure(&self, root: PathBuf) -> Result<(), String> {
        fs::create_dir_all(&root)
            .map_err(|error| format!("failed to create native media IPC directory: {error}"))?;
        for entry in fs::read_dir(&root)
            .map_err(|error| format!("failed to inspect native media IPC directory: {error}"))?
        {
            let entry = entry
                .map_err(|error| format!("failed to inspect native media IPC entry: {error}"))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let file_type = entry.file_type().map_err(|error| {
                format!("failed to inspect native media IPC entry type: {error}")
            })?;
            if file_type.is_file() && name.starts_with(".risunest-native-media-") {
                fs::remove_file(entry.path()).map_err(|error| {
                    format!("failed to remove stale native media IPC file: {error}")
                })?;
            }
        }
        *self
            .spool_root
            .lock()
            .map_err(|error| format!("native media IPC root mutex poisoned: {error}"))? =
            Some(root);
        Ok(())
    }

    fn create_spool(&self) -> Result<tempfile::NamedTempFile, String> {
        let root = self
            .spool_root
            .lock()
            .map_err(|error| format!("native media IPC root mutex poisoned: {error}"))?
            .clone()
            .ok_or_else(|| "native media IPC directory is unavailable".to_owned())?;
        tempfile::Builder::new()
            .prefix(".risunest-native-media-")
            .tempfile_in(root)
            .map_err(|error| format!("failed to create native media IPC spool: {error}"))
    }

    pub(crate) fn reset_renderer_session(&self) -> Result<(), String> {
        self.pool
            .lock()
            .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
            .reset_renderer_session();
        Ok(())
    }
}

fn read_processing_input(mut input: ProcessingInput) -> Result<(ProcessingInput, Vec<u8>), String> {
    input
        .file
        .seek(SeekFrom::Start(0))
        .map_err(|error| format!("failed to seek native media input: {error}"))?;
    let mut data = Vec::with_capacity(input.total_bytes as usize);
    input
        .file
        .read_to_end(&mut data)
        .map_err(|error| format!("failed to read native media input: {error}"))?;
    if data.len() as u64 != input.total_bytes {
        return Err("native media input length changed during processing".to_owned());
    }
    Ok((input, data))
}

fn finish_encoded_result(
    state: &NativeMediaIpcState,
    id: String,
    generation: u64,
    encoded: EncodedInlayImage,
) -> Result<EncodedInlayIpcResult, String> {
    let EncodedInlayImage { data, metadata } = encoded;
    let output_size = data.len() as u64;
    {
        let mut pool = state
            .pool
            .lock()
            .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?;
        if let Err(error) = pool.reserve_encoded_output(&id, generation, output_size) {
            pool.finish_processing(&id);
            return Err(error);
        }
    }
    if data.len() <= NATIVE_MEDIA_IPC_CHUNK_BYTES {
        state
            .pool
            .lock()
            .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
            .complete_inline(&id, generation)?;
        return Ok(EncodedInlayIpcResult {
            data: Some(data),
            output_id: None,
            output_size,
            metadata,
        });
    }
    let mut file = match state.create_spool() {
        Ok(file) => file,
        Err(error) => {
            if let Ok(mut pool) = state.pool.lock() {
                pool.finish_processing(&id);
            }
            return Err(format!(
                "failed to create native media output spool: {error}"
            ));
        }
    };
    if let Err(error) = file.write_all(&data).and_then(|()| file.flush()) {
        if let Ok(mut pool) = state.pool.lock() {
            pool.finish_processing(&id);
        }
        return Err(format!(
            "failed to write native media output spool: {error}"
        ));
    }
    let output_id = uuid::Uuid::new_v4().to_string();
    state
        .pool
        .lock()
        .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
        .complete_download(&id, output_id.clone(), generation, output_size, file)?;
    Ok(EncodedInlayIpcResult {
        data: None,
        output_id: Some(output_id),
        output_size,
        metadata,
    })
}

#[tauri::command(async)]
pub(crate) async fn native_media_inlay_input_open(
    app: AppHandle,
    upload_id: String,
    total_bytes: u64,
) -> Result<NativeMediaInputOpened, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        let state = app.state::<NativeMediaIpcState>();
        let file = state.create_spool()?;
        state
            .pool
            .lock()
            .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
            .open(upload_id, total_bytes, file)?;
        Ok(NativeMediaInputOpened {
            capacity: NATIVE_MEDIA_IPC_CHUNK_BYTES,
        })
    })
    .await
    .map_err(|error| format!("failed to join native media input open operation: {error}"))?
}

#[tauri::command(async)]
pub(crate) async fn native_media_inlay_input_chunk(
    app: AppHandle,
    upload_id: String,
    offset: u64,
    data: Vec<u8>,
) -> Result<u64, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        app.state::<NativeMediaIpcState>()
            .pool
            .lock()
            .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
            .append(&upload_id, offset, &data)
    })
    .await
    .map_err(|error| format!("failed to join native media input chunk operation: {error}"))?
}

#[tauri::command]
pub(crate) fn native_media_inlay_input_cancel(
    state: tauri::State<'_, NativeMediaIpcState>,
    upload_id: String,
) -> Result<(), String> {
    state
        .pool
        .lock()
        .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
        .cancel_renderer(&upload_id);
    Ok(())
}

#[tauri::command(async)]
pub(crate) async fn native_media_encode_inlay_finish(
    app: AppHandle,
    upload_id: String,
    id: String,
    name: String,
    options: Option<InlayEncodeOptions>,
) -> Result<EncodedInlayIpcResult, String> {
    let cleanup_app = app.clone();
    let cleanup_id = upload_id.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        let state = app.state::<NativeMediaIpcState>();
        let input = state
            .pool
            .lock()
            .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
            .begin_uploaded_processing(&upload_id)?;
        let _admission = state.admit_processing(&upload_id)?;
        let (input, data) = match read_processing_input(input) {
            Ok(value) => value,
            Err(error) => {
                if let Ok(mut pool) = state.pool.lock() {
                    pool.finish_processing(&upload_id);
                }
                return Err(error);
            }
        };
        let encoded = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            encode_inlay_image(&id, &data, &name, options)
        })) {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                if let Ok(mut pool) = state.pool.lock() {
                    pool.finish_processing(&input.id);
                }
                return Err(error);
            }
            Err(_) => {
                if let Ok(mut pool) = state.pool.lock() {
                    pool.finish_processing(&input.id);
                }
                return Err("native Inlay encoder panicked".to_owned());
            }
        };
        let processing_id = input.id;
        let generation = input.generation;
        drop(input.file);
        finish_encoded_result(&state, processing_id, generation, encoded)
    })
    .await;
    match result {
        Ok(result) => result,
        Err(error) => {
            if let Some(state) = cleanup_app.try_state::<NativeMediaIpcState>() {
                if let Ok(mut pool) = state.pool.lock() {
                    pool.finish_processing(&cleanup_id);
                }
            }
            Err(format!(
                "failed to join native Inlay streamed encoder: {error}"
            ))
        }
    }
}

#[tauri::command(async)]
pub(crate) async fn native_media_write_inlay_finish(
    app: AppHandle,
    upload_id: String,
    id: String,
    name: String,
    options: Option<InlayEncodeOptions>,
) -> Result<InlayImageMetadata, String> {
    let cleanup_app = app.clone();
    let cleanup_id = upload_id.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        app.state::<crate::NativeStartupState>().ensure_ready()?;
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        let state = app.state::<NativeMediaIpcState>();
        let input = state
            .pool
            .lock()
            .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
            .begin_uploaded_processing(&upload_id)?;
        let _admission = state.admit_processing(&upload_id)?;
        let (input, data) = match read_processing_input(input) {
            Ok(value) => value,
            Err(error) => {
                if let Ok(mut pool) = state.pool.lock() {
                    pool.finish_processing(&upload_id);
                }
                return Err(error);
            }
        };
        let result = (|| {
            drop(input.file);
            let root = crate::app_data_root::resolve(&app).map_err(|error| {
                format!("failed to resolve application data directory: {error}")
            })?;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                write_inlay_image_with_options(&root, &id, &data, &name, options)
            }))
            .unwrap_or_else(|_| Err("native Inlay writer panicked".to_owned()))
        })();
        if let Ok(mut pool) = state.pool.lock() {
            pool.finish_processing(&upload_id);
        }
        result
    })
    .await;
    match result {
        Ok(result) => result,
        Err(error) => {
            if let Some(state) = cleanup_app.try_state::<NativeMediaIpcState>() {
                if let Ok(mut pool) = state.pool.lock() {
                    pool.finish_processing(&cleanup_id);
                }
            }
            Err(format!(
                "failed to join native Inlay streamed writer: {error}"
            ))
        }
    }
}

#[tauri::command(async)]
pub(crate) async fn native_media_inlay_output_read(
    app: AppHandle,
    output_id: String,
    start: u64,
    end_exclusive: u64,
) -> Result<Vec<u8>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let _operation = app
            .state::<PersistentStoreState>()
            .admit_renderer_operation()
            .map_err(|error| error.to_string())?;
        app.state::<NativeMediaIpcState>()
            .pool
            .lock()
            .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
            .read_download(&output_id, start, end_exclusive)
    })
    .await
    .map_err(|error| format!("failed to join native media output read operation: {error}"))?
}

#[tauri::command]
pub(crate) fn native_media_inlay_output_cancel(
    state: tauri::State<'_, NativeMediaIpcState>,
    output_id: String,
) -> Result<(), String> {
    state
        .pool
        .lock()
        .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
        .cancel_renderer(&output_id);
    Ok(())
}

pub(super) fn encode_direct(
    state: &NativeMediaIpcState,
    id: &str,
    data: &[u8],
    name: &str,
    options: Option<InlayEncodeOptions>,
) -> Result<EncodedInlayIpcResult, String> {
    if data.len() > NATIVE_MEDIA_IPC_CHUNK_BYTES {
        return Err("native Inlay direct encoder input exceeds one IPC chunk".to_owned());
    }
    let (output_id, generation) = state
        .pool
        .lock()
        .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
        .begin_direct_processing(data.len() as u64)?;
    let _admission = state.admit_processing(&output_id)?;
    let encoded = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        encode_inlay_image(id, data, name, options)
    })) {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            if let Ok(mut pool) = state.pool.lock() {
                pool.finish_processing(&output_id);
            }
            return Err(error);
        }
        Err(_) => {
            if let Ok(mut pool) = state.pool.lock() {
                pool.finish_processing(&output_id);
            }
            return Err("native Inlay encoder panicked".to_owned());
        }
    };
    finish_encoded_result(state, output_id, generation, encoded)
}

pub(super) fn write_direct(
    state: &NativeMediaIpcState,
    root: &Path,
    id: &str,
    data: &[u8],
    name: &str,
    options: Option<InlayEncodeOptions>,
) -> Result<InlayImageMetadata, String> {
    if data.len() > NATIVE_MEDIA_IPC_CHUNK_BYTES {
        return Err("native Inlay direct writer input exceeds one IPC chunk".to_owned());
    }
    let (processing_id, _) = state
        .pool
        .lock()
        .map_err(|error| format!("native media transfer mutex poisoned: {error}"))?
        .begin_direct_processing(data.len() as u64)?;
    let _admission = state.admit_processing(&processing_id)?;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        write_inlay_image_with_options(root, id, data, name, options)
    }))
    .unwrap_or_else(|_| Err("native Inlay writer panicked".to_owned()));
    if let Ok(mut pool) = state.pool.lock() {
        pool.finish_processing(&processing_id);
    }
    result
}

#[cfg(test)]
mod tests {
    #[test]
    fn queued_direct_encoders_do_not_start_before_admission_and_cancel_without_leaking() {
        let state = super::NativeMediaIpcState::default();
        let admission = state.processing.lock().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            for _ in 0..2 {
                let send = send.clone();
                let state = &state;
                scope.spawn(move || {
                    send.send(super::encode_direct(state, "synthetic", b"original", "file.bin", None)).unwrap();
                });
            }
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while state.pool.lock().unwrap().transfers.len() != 2 {
                assert!(std::time::Instant::now() < deadline, "workers did not reserve their inputs");
                std::thread::yield_now();
            }
            assert!(receive.try_recv().is_err());
            state.reset_renderer_session().unwrap();
            drop(admission);
            for _ in 0..2 {
                assert!(receive.recv().unwrap().err().expect("cancelled operation").contains("cancelled"));
            }
        });
        assert!(state.pool.lock().unwrap().transfers.is_empty());
        assert_eq!(state.pool.lock().unwrap().reserved_bytes, 0);
        let result = super::encode_direct(&state, "next", b"original", "file.bin", None).unwrap();
        assert_eq!(result.data.as_deref(), Some(b"original".as_slice()));
    }

    #[test]
    fn poisoned_processing_admission_releases_the_reserved_slot() {
        let state = super::NativeMediaIpcState::default();
        let _ = std::panic::catch_unwind(|| {
            let _guard = state.processing.lock().unwrap();
            panic!("synthetic worker failure");
        });
        assert!(super::encode_direct(&state, "synthetic", b"original", "file.bin", None).is_err());
        assert!(state.pool.lock().unwrap().transfers.is_empty());
        assert_eq!(state.pool.lock().unwrap().reserved_bytes, 0);
    }

    use super::*;
    use crate::native_media::InlayEncodeFormat;
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;

    fn spool() -> tempfile::NamedTempFile {
        tempfile::NamedTempFile::new().unwrap()
    }

    #[test]
    fn upload_enforces_offsets_lengths_capacity_and_reset_cleanup() {
        let mut pool = TransferPool::default();
        let id = uuid::Uuid::new_v4().to_string();
        pool.open(id.clone(), NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 1, spool())
            .unwrap();
        assert!(pool.append(&id, 1, &[1]).is_err());
        assert!(pool
            .append(&id, 0, &vec![1; NATIVE_MEDIA_IPC_CHUNK_BYTES + 1])
            .is_err());
        assert_eq!(
            pool.append(&id, 0, &vec![1; NATIVE_MEDIA_IPC_CHUNK_BYTES])
                .unwrap(),
            NATIVE_MEDIA_IPC_CHUNK_BYTES as u64
        );
        assert!(pool
            .append(&id, NATIVE_MEDIA_IPC_CHUNK_BYTES as u64, &[2, 3])
            .is_err());
        assert_eq!(
            pool.append(&id, NATIVE_MEDIA_IPC_CHUNK_BYTES as u64, &[2])
                .unwrap(),
            NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 1
        );
        assert_eq!(pool.transfers.len(), 1);
        pool.reset_renderer_session();
        assert!(pool.transfers.is_empty());
        assert_eq!(pool.reserved_bytes, 0);
    }

    #[test]
    fn processing_survives_until_output_transition_and_old_generation_cannot_reinsert() {
        let mut pool = TransferPool::default();
        let id = uuid::Uuid::new_v4().to_string();
        pool.open(id.clone(), NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 1, spool())
            .unwrap();
        pool.append(&id, 0, &vec![1; NATIVE_MEDIA_IPC_CHUNK_BYTES])
            .unwrap();
        pool.append(&id, NATIVE_MEDIA_IPC_CHUNK_BYTES as u64, &[2])
            .unwrap();
        let input = pool.begin_uploaded_processing(&id).unwrap();
        assert!(matches!(
            pool.transfers.get(&id),
            Some(Transfer::Processing(_))
        ));
        pool.reset_renderer_session();
        let output = spool();
        let output_id = uuid::Uuid::new_v4().to_string();
        assert!(pool
            .complete_download(&input.id, output_id, input.generation, 70_000, output)
            .is_err());
        assert!(pool.transfers.is_empty());
        assert_eq!(pool.reserved_bytes, 0);
    }

    #[test]
    fn download_ranges_are_sequential_and_bounded() {
        let mut pool = TransferPool::default();
        let (id, generation) = pool.begin_direct_processing(3).unwrap();
        let mut file = spool();
        file.write_all(&vec![7; NATIVE_MEDIA_IPC_CHUNK_BYTES + 2])
            .unwrap();
        pool.reserve_encoded_output(&id, generation, NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 2)
            .unwrap();
        pool.complete_download(
            &id,
            id.clone(),
            generation,
            NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 2,
            file,
        )
        .unwrap();
        assert_eq!(
            pool.read_download(&id, 0, NATIVE_MEDIA_IPC_CHUNK_BYTES as u64)
                .unwrap()
                .len(),
            NATIVE_MEDIA_IPC_CHUNK_BYTES
        );
        assert_eq!(
            pool.read_download(
                &id,
                NATIVE_MEDIA_IPC_CHUNK_BYTES as u64,
                NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 2,
            )
            .unwrap(),
            vec![7, 7]
        );
        pool.cancel(&id);
        assert_eq!(pool.reserved_bytes, 0);

        let (wrong_offset_id, generation) = pool.begin_direct_processing(1).unwrap();
        let mut file = spool();
        file.write_all(&[1, 2]).unwrap();
        pool.reserve_encoded_output(&wrong_offset_id, generation, 2)
            .unwrap();
        pool.complete_download(
            &wrong_offset_id,
            wrong_offset_id.clone(),
            generation,
            2,
            file,
        )
        .unwrap();
        assert!(pool.read_download(&wrong_offset_id, 1, 2).is_err());
        assert!(!pool.transfers.contains_key(&wrong_offset_id));

        let (oversized_id, generation) = pool.begin_direct_processing(1).unwrap();
        let mut file = spool();
        file.write_all(&vec![1; NATIVE_MEDIA_IPC_CHUNK_BYTES + 1])
            .unwrap();
        pool.reserve_encoded_output(
            &oversized_id,
            generation,
            NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 1,
        )
        .unwrap();
        pool.complete_download(
            &oversized_id,
            oversized_id.clone(),
            generation,
            NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 1,
            file,
        )
        .unwrap();
        assert!(pool
            .read_download(&oversized_id, 0, NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 1,)
            .is_err());
        assert!(!pool.transfers.contains_key(&oversized_id));
    }

    #[test]
    fn enforces_transfer_count_and_reserved_byte_quotas() {
        let mut pool = TransferPool::default();
        for _ in 0..MAX_CONCURRENT_NATIVE_MEDIA_TRANSFERS {
            pool.open(
                uuid::Uuid::new_v4().to_string(),
                NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 1,
                spool(),
            )
            .unwrap();
        }
        assert!(pool
            .open(
                uuid::Uuid::new_v4().to_string(),
                NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 1,
                spool(),
            )
            .is_err());

        let mut quota_pool = TransferPool::default();
        quota_pool
            .open(
                uuid::Uuid::new_v4().to_string(),
                MAX_NATIVE_MEDIA_TRANSFER_BYTES,
                spool(),
            )
            .unwrap();
        quota_pool
            .open(
                uuid::Uuid::new_v4().to_string(),
                MAX_NATIVE_MEDIA_TRANSFER_BYTES,
                spool(),
            )
            .unwrap();
        assert!(quota_pool
            .open(
                uuid::Uuid::new_v4().to_string(),
                NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 1,
                spool(),
            )
            .is_err());
        assert_eq!(
            quota_pool.reserved_bytes,
            MAX_TOTAL_NATIVE_MEDIA_TRANSFER_BYTES
        );
    }

    #[test]
    fn oversized_encoded_output_releases_processing_reservation() {
        let mut pool = TransferPool::default();
        let (id, generation) = pool.begin_direct_processing(1).unwrap();
        let file = spool();

        assert!(pool
            .complete_download(
                &id,
                id.clone(),
                generation,
                MAX_NATIVE_MEDIA_TRANSFER_BYTES + 1,
                file,
            )
            .is_err());
        assert!(pool.transfers.is_empty());
        assert_eq!(pool.reserved_bytes, 0);
    }

    #[test]
    fn renderer_cancel_keeps_processing_capacity_reserved_until_worker_finishes() {
        let mut pool = TransferPool::default();
        let (id, _) = pool.begin_direct_processing(7).unwrap();

        pool.cancel_renderer(&id);

        assert!(matches!(
            pool.transfers.get(&id),
            Some(Transfer::Processing(Processing {
                cancelled: true,
                ..
            }))
        ));
        assert_eq!(pool.reserved_bytes, 7);
        pool.finish_processing(&id);
        assert!(pool.transfers.is_empty());
        assert_eq!(pool.reserved_bytes, 0);
    }

    #[test]
    fn wrong_phase_output_read_does_not_cancel_processing() {
        let mut pool = TransferPool::default();
        let (id, _) = pool.begin_direct_processing(9).unwrap();

        assert!(pool.read_download(&id, 0, 1).is_err());

        assert!(matches!(
            pool.transfers.get(&id),
            Some(Transfer::Processing(Processing {
                cancelled: false,
                ..
            }))
        ));
        assert_eq!(pool.reserved_bytes, 9);
    }

    #[test]
    fn duplicate_finish_does_not_cancel_the_active_worker() {
        let mut pool = TransferPool::default();
        let id = uuid::Uuid::new_v4().to_string();
        pool.open(id.clone(), NATIVE_MEDIA_IPC_CHUNK_BYTES as u64 + 1, spool())
            .unwrap();
        pool.append(&id, 0, &vec![1; NATIVE_MEDIA_IPC_CHUNK_BYTES])
            .unwrap();
        pool.append(&id, NATIVE_MEDIA_IPC_CHUNK_BYTES as u64, &[2])
            .unwrap();
        pool.begin_uploaded_processing(&id).unwrap();

        assert!(pool.begin_uploaded_processing(&id).is_err());
        assert!(matches!(
            pool.transfers.get(&id),
            Some(Transfer::Processing(Processing {
                cancelled: false,
                ..
            }))
        ));
    }

    #[test]
    fn encoded_output_quota_is_reserved_before_spooling() {
        let mut pool = TransferPool::default();
        let first = uuid::Uuid::new_v4().to_string();
        let second = uuid::Uuid::new_v4().to_string();
        pool.open(first, MAX_NATIVE_MEDIA_TRANSFER_BYTES, spool())
            .unwrap();
        pool.open(second, MAX_NATIVE_MEDIA_TRANSFER_BYTES - 1, spool())
            .unwrap();
        let (processing_id, generation) = pool.begin_direct_processing(1).unwrap();

        assert!(pool
            .reserve_encoded_output(&processing_id, generation, 2)
            .is_err());
        assert_eq!(pool.reserved_bytes, MAX_TOTAL_NATIVE_MEDIA_TRANSFER_BYTES);
        assert!(matches!(
            pool.transfers.get(&processing_id),
            Some(Transfer::Processing(Processing {
                reserved_bytes: 1,
                ..
            }))
        ));
    }

    #[test]
    fn configure_removes_only_recognized_stale_spools() {
        let directory = tempfile::tempdir().unwrap();
        let stale = directory.path().join(".risunest-native-media-stale");
        let unrelated = directory.path().join("keep.txt");
        fs::write(&stale, b"stale").unwrap();
        fs::write(&unrelated, b"keep").unwrap();
        let state = NativeMediaIpcState::default();

        state.configure(directory.path().to_owned()).unwrap();
        let spool = state.create_spool().unwrap();

        assert!(!stale.exists());
        assert!(unrelated.exists());
        assert_eq!(spool.path().parent(), Some(directory.path()));
        assert!(spool
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".risunest-native-media-"));
    }

    #[test]
    fn streamed_original_image_round_trips_through_the_product_encoder() {
        let directory = tempfile::tempdir().unwrap();
        let state = NativeMediaIpcState::default();
        state.configure(directory.path().to_owned()).unwrap();
        let image = DynamicImage::ImageRgba8(RgbaImage::from_fn(320, 320, |x, y| {
            let value = x
                .wrapping_mul(0x9e37_79b9)
                .rotate_left(y % 31)
                .wrapping_add(y.wrapping_mul(0x85eb_ca6b));
            Rgba([value as u8, (value >> 8) as u8, (value >> 16) as u8, 255])
        }));
        let mut encoded = Cursor::new(Vec::new());
        image.write_to(&mut encoded, ImageFormat::Png).unwrap();
        let source = encoded.into_inner();
        assert!(source.len() > NATIVE_MEDIA_IPC_CHUNK_BYTES);
        let upload_id = uuid::Uuid::new_v4().to_string();
        state
            .pool
            .lock()
            .unwrap()
            .open(
                upload_id.clone(),
                source.len() as u64,
                state.create_spool().unwrap(),
            )
            .unwrap();
        for (index, chunk) in source.chunks(NATIVE_MEDIA_IPC_CHUNK_BYTES).enumerate() {
            state
                .pool
                .lock()
                .unwrap()
                .append(
                    &upload_id,
                    (index * NATIVE_MEDIA_IPC_CHUNK_BYTES) as u64,
                    chunk,
                )
                .unwrap();
        }
        let input = state
            .pool
            .lock()
            .unwrap()
            .begin_uploaded_processing(&upload_id)
            .unwrap();
        let (input, uploaded) = read_processing_input(input).unwrap();
        assert_eq!(uploaded, source);
        let processing_id = input.id;
        let generation = input.generation;
        drop(input.file);
        let result = finish_encoded_result(
            &state,
            processing_id,
            generation,
            encode_inlay_image(
                "synthetic-round-trip",
                &uploaded,
                "synthetic.png",
                Some(InlayEncodeOptions {
                    format: InlayEncodeFormat::Original,
                    ..InlayEncodeOptions::default()
                }),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(result.data.is_none());
        assert_eq!(result.output_size, source.len() as u64);
        let output_id = result.output_id.unwrap();
        let mut downloaded = Vec::with_capacity(source.len());
        for start in (0..source.len()).step_by(NATIVE_MEDIA_IPC_CHUNK_BYTES) {
            let end = (start + NATIVE_MEDIA_IPC_CHUNK_BYTES).min(source.len());
            downloaded.extend(
                state
                    .pool
                    .lock()
                    .unwrap()
                    .read_download(&output_id, start as u64, end as u64)
                    .unwrap(),
            );
        }
        assert_eq!(downloaded, source);
        state.pool.lock().unwrap().cancel_renderer(&output_id);
        assert_eq!(state.pool.lock().unwrap().reserved_bytes, 0);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }
}
