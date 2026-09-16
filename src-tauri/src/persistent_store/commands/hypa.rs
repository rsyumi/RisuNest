//! Renderer access to the embedding cache. Both directions carry one framed
//! payload: a little endian `u32` header length, a JSON header, then the
//! Float32 little endian vectors concatenated in header order.
use super::{with_store, with_store_mut, PersistentStoreState};
use crate::persistent_store::{
    device_store::hypa::{HypaEmbeddingWrite, MAX_BATCH_ENTRIES, VECTOR_ELEMENT_BYTES},
    StoreError, StoreResult,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::{
    ipc::{InvokeBody, Request, Response},
    AppHandle, State,
};

const HEADER_LENGTH_BYTES: usize = 4;
const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

fn invalid(message: &str) -> StoreError {
    StoreError::Validation {
        message: message.to_owned(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteHeaderEntry {
    key: String,
    producer: String,
    model: String,
    endpoint: Option<String>,
    preprocess_version: i64,
    dimensions: i64,
    byte_length: usize,
    metadata: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WriteHeader {
    entries: Vec<WriteHeaderEntry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadHeaderEntry {
    key: String,
    dimensions: i64,
    byte_length: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadHeader {
    entries: Vec<ReadHeaderEntry>,
}

/// Splits a framed payload into its JSON header and the vector region.
fn split_frame(bytes: &[u8]) -> StoreResult<(&[u8], &[u8])> {
    if bytes.len() < HEADER_LENGTH_BYTES || bytes.len() > MAX_FRAME_BYTES {
        return Err(invalid("embedding frame is malformed"));
    }
    let header_length = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    let body_start = HEADER_LENGTH_BYTES
        .checked_add(header_length)
        .ok_or_else(|| invalid("embedding frame is malformed"))?;
    if body_start > bytes.len() {
        return Err(invalid("embedding frame is malformed"));
    }
    Ok((&bytes[HEADER_LENGTH_BYTES..body_start], &bytes[body_start..]))
}

fn build_frame(header: &ReadHeader, vectors: Vec<Vec<u8>>) -> StoreResult<Vec<u8>> {
    let encoded = serde_json::to_vec(header).map_err(|_| invalid("embedding frame is malformed"))?;
    let length = u32::try_from(encoded.len()).map_err(|_| invalid("embedding frame is too big"))?;
    let body_bytes = vectors.iter().map(Vec::len).sum::<usize>();
    let mut frame = Vec::with_capacity(HEADER_LENGTH_BYTES + encoded.len() + body_bytes);
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&encoded);
    for vector in vectors {
        frame.extend_from_slice(&vector);
    }
    Ok(frame)
}

fn decode_write_frame(bytes: &[u8]) -> StoreResult<Vec<HypaEmbeddingWrite>> {
    let (header_bytes, body) = split_frame(bytes)?;
    let header: WriteHeader =
        serde_json::from_slice(header_bytes).map_err(|_| invalid("embedding header is invalid"))?;
    if header.entries.len() > MAX_BATCH_ENTRIES {
        return Err(invalid("embedding write batch is too large"));
    }
    let mut offset = 0usize;
    let mut entries = Vec::with_capacity(header.entries.len());
    for entry in header.entries {
        if entry.dimensions <= 0
            || entry.byte_length != entry.dimensions as usize * VECTOR_ELEMENT_BYTES
        {
            return Err(invalid("embedding vector length does not match dimensions"));
        }
        let end = offset
            .checked_add(entry.byte_length)
            .ok_or_else(|| invalid("embedding frame is malformed"))?;
        if end > body.len() {
            return Err(invalid("embedding frame is truncated"));
        }
        entries.push(HypaEmbeddingWrite {
            cache_key: entry.key,
            producer: entry.producer,
            model: entry.model,
            endpoint: entry.endpoint,
            preprocess_version: entry.preprocess_version,
            dimensions: entry.dimensions,
            vector: body[offset..end].to_vec(),
            metadata: entry.metadata,
        });
        offset = end;
    }
    if offset != body.len() {
        return Err(invalid("embedding frame has trailing bytes"));
    }
    Ok(entries)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EncodedWrite {
    payload: String,
}

/// Android cannot deliver a raw request body, so it sends the same frame as
/// base64 instead of a number array.
fn request_frame(request: &Request<'_>) -> StoreResult<Vec<u8>> {
    match request.body() {
        InvokeBody::Raw(bytes) => Ok(bytes.clone()),
        InvokeBody::Json(value) => {
            let encoded: EncodedWrite = serde_json::from_value(value.clone())
                .map_err(|_| invalid("embedding write payload is invalid"))?;
            base64::engine::general_purpose::STANDARD
                .decode(encoded.payload.as_bytes())
                .map_err(|_| invalid("embedding write payload is invalid"))
        }
    }
}

#[tauri::command(async)]
pub(crate) fn pds_read_hypa_embeddings(
    state: State<'_, PersistentStoreState>,
    keys: Vec<String>,
) -> StoreResult<Response> {
    let rows = with_store(state, |store| {
        store.device_store()?.read_hypa_embeddings(&keys)
    })?;
    let mut entries = Vec::with_capacity(rows.len());
    let mut vectors = Vec::with_capacity(rows.len());
    for row in rows {
        let vector = row.vector.unwrap_or_default();
        entries.push(ReadHeaderEntry {
            key: row.cache_key,
            dimensions: row.dimensions,
            byte_length: vector.len(),
        });
        vectors.push(vector);
    }
    Ok(Response::new(build_frame(&ReadHeader { entries }, vectors)?))
}

#[tauri::command(async)]
pub(crate) fn pds_write_hypa_embeddings(
    app: AppHandle,
    state: State<'_, PersistentStoreState>,
    request: Request<'_>,
) -> StoreResult<()> {
    let entries = decode_write_frame(&request_frame(&request)?)?;
    let changed = with_store_mut(state, |store| {
        let device = store.device_store_mut()?;
        let before = device.revision()?;
        device.write_hypa_embeddings(&entries)?;
        Ok(device.revision()? != before)
    })?;
    if changed {
        crate::server_sync::events::notify_device_changed(&app);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
