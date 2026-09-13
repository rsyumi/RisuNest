use super::{
    character_json_export::{self, JsonAssetSource, FALLBACK_PORTRAIT},
    error::{cancelled, destination_error_with, invalid_input, io_error, job_error, store_error},
    JobControl, JobPhase, JobProgress, JobResultSummary, NativeJobError,
};
use crate::asset_repository::PayloadCas;
use crate::persistent_store::{
    export::{self, destination},
    PreparedRisuSaveExport, StoreResult,
};
use crate::server_sync::residency::RemotePayloadAccess;
use base64::{engine::general_purpose::STANDARD, write::EncoderWriter, Engine as _};
use image::ImageEncoder;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fs::{self, OpenOptions};
use std::io::{self, BufWriter, Cursor, Read, Write};
use std::path::Path;
use uuid::Uuid;

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAX_PNG_CHUNK_BYTES: usize = i32::MAX as usize;
const MAX_EMBEDDED_ASSET_BYTES: u64 = 50 * 1024 * 1024;
const MAX_EMBEDDED_ASSET_COUNT: u64 = 10_000;
const MAX_EMBEDDED_ASSET_TOTAL_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Clone, Copy)]
struct PngExportLimits {
    max_input_bytes: usize,
    max_dimension: u32,
    max_pixels: u64,
    max_alloc: u64,
}

impl Default for PngExportLimits {
    fn default() -> Self {
        Self {
            max_input_bytes: 50 * 1024 * 1024,
            max_dimension: 8_192,
            max_pixels: 40_000_000,
            max_alloc: 256 * 1024 * 1024,
        }
    }
}

#[derive(Clone)]
struct ParsedPngChunk {
    kind: [u8; 4],
    data: Vec<u8>,
}

fn parse_chunks(bytes: &[u8]) -> Result<Vec<ParsedPngChunk>, NativeJobError> {
    if bytes.len() < PNG_SIGNATURE.len() || &bytes[..PNG_SIGNATURE.len()] != PNG_SIGNATURE {
        return Err(invalid_input("character portrait is not a PNG"));
    }
    let mut offset = PNG_SIGNATURE.len();
    let mut chunks = Vec::new();
    let mut seen_ihdr = false;
    let mut seen_idat = false;
    while offset < bytes.len() {
        let header_end = offset
            .checked_add(8)
            .ok_or_else(|| invalid_input("PNG chunk offset overflow"))?;
        if header_end > bytes.len() {
            return Err(invalid_input("truncated PNG chunk header"));
        }
        let length = u32::from_be_bytes(bytes[offset..offset + 4].try_into().unwrap()) as usize;
        if length > MAX_PNG_CHUNK_BYTES {
            return Err(invalid_input("PNG chunk exceeds its length limit"));
        }
        let kind: [u8; 4] = bytes[offset + 4..header_end].try_into().unwrap();
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return Err(invalid_input("PNG chunk type contains non-letter bytes"));
        }
        let data_end = header_end
            .checked_add(length)
            .ok_or_else(|| invalid_input("PNG chunk length overflow"))?;
        let chunk_end = data_end
            .checked_add(4)
            .ok_or_else(|| invalid_input("PNG chunk length overflow"))?;
        if chunk_end > bytes.len() {
            return Err(invalid_input("truncated PNG chunk data"));
        }
        if chunks.is_empty() && kind != *b"IHDR" {
            return Err(invalid_input("PNG IHDR must be first"));
        }
        if kind == *b"IHDR" {
            if seen_ihdr || length != 13 {
                return Err(invalid_input("invalid PNG IHDR"));
            }
            seen_ihdr = true;
        }
        if kind == *b"IDAT" {
            seen_idat = true;
        }
        if kind == *b"IEND" && length != 0 {
            return Err(invalid_input("invalid PNG IEND"));
        }
        let mut crc = crc32fast::Hasher::new();
        crc.update(&kind);
        crc.update(&bytes[header_end..data_end]);
        let expected = u32::from_be_bytes(bytes[data_end..chunk_end].try_into().unwrap());
        if crc.finalize() != expected {
            return Err(invalid_input(format!(
                "PNG chunk CRC mismatch for {}",
                String::from_utf8_lossy(&kind)
            )));
        }
        chunks.push(ParsedPngChunk {
            kind,
            data: bytes[header_end..data_end].to_vec(),
        });
        offset = chunk_end;
        if kind == *b"IEND" {
            if offset != bytes.len() {
                return Err(invalid_input("PNG contains data after IEND"));
            }
            break;
        }
    }
    if !seen_ihdr || !seen_idat || chunks.last().map(|chunk| chunk.kind) != Some(*b"IEND") {
        return Err(invalid_input("PNG is missing required image chunks"));
    }
    Ok(chunks)
}

fn prepare_png_portrait(
    bytes: &[u8],
    limits: PngExportLimits,
    is_cancelled: impl Fn() -> bool,
) -> Result<Vec<u8>, NativeJobError> {
    if is_cancelled() {
        return Err(cancelled("PNG export cancelled before portrait decoding"));
    }
    if bytes.len() > limits.max_input_bytes {
        return Err(invalid_input(
            "character portrait exceeds the PNG input limit",
        ));
    }
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| invalid_input(format!("character portrait is invalid: {error}")))?;
    let source_format = reader
        .format()
        .ok_or_else(|| invalid_input("character portrait format is unsupported"))?;
    let mut image_limits = image::Limits::default();
    image_limits.max_image_width = Some(limits.max_dimension);
    image_limits.max_image_height = Some(limits.max_dimension);
    image_limits.max_alloc = Some(limits.max_alloc);
    reader.limits(image_limits);
    let decoder = reader
        .into_decoder()
        .map_err(|error| invalid_input(format!("character portrait is invalid: {error}")))?;
    let (width, height) = image::ImageDecoder::dimensions(&decoder);
    if width == 0
        || height == 0
        || width > limits.max_dimension
        || height > limits.max_dimension
        || u64::from(width).saturating_mul(u64::from(height)) > limits.max_pixels
    {
        return Err(invalid_input(
            "character portrait dimensions exceed the PNG limit",
        ));
    }
    let decoded = image::DynamicImage::from_decoder(decoder)
        .map_err(|error| invalid_input(format!("character portrait is invalid: {error}")))?;
    if is_cancelled() {
        return Err(cancelled("PNG export cancelled before portrait encoding"));
    }
    if source_format == image::ImageFormat::Png {
        parse_chunks(bytes)?;
        return Ok(bytes.to_vec());
    }
    let rgba = decoded.to_rgba8();
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            rgba.as_raw(),
            width,
            height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| invalid_input(format!("character portrait cannot be encoded: {error}")))?;
    if png.len() > limits.max_input_bytes {
        return Err(invalid_input("encoded PNG portrait exceeds its byte limit"));
    }
    Ok(png)
}

#[cfg(test)]
fn write_png_card(
    portrait: &mut impl Read,
    output: &mut impl Write,
    metadata: &[u8],
    assets: &[(String, Vec<u8>)],
    is_cancelled: impl Fn() -> bool,
) -> Result<(), NativeJobError> {
    let mut bytes = Vec::new();
    portrait
        .take((PngExportLimits::default().max_input_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    let bytes = prepare_png_portrait(&bytes, PngExportLimits::default(), &is_cancelled)?;
    let chunks = parse_chunks(&bytes)?;
    output.write_all(PNG_SIGNATURE).map_err(io_error)?;
    for chunk in chunks {
        if is_cancelled() {
            return Err(cancelled("PNG export cancelled while writing chunks"));
        }
        if chunk.kind == *b"IEND" {
            write_text_chunk(output, "ccv3", &STANDARD.encode(metadata))?;
            for (reference, payload) in assets {
                if reference.is_empty() || reference.as_bytes().len() > 60 {
                    return Err(invalid_input("PNG asset reference is invalid"));
                }
                write_text_chunk(
                    output,
                    &format!("chara-ext-asset_:{reference}"),
                    &STANDARD.encode(payload),
                )?;
            }
            write_chunk(output, &chunk.kind, &chunk.data)?;
            continue;
        }
        if chunk.kind == *b"tEXt" && owned_text_chunk(&chunk.data) {
            continue;
        }
        write_chunk(output, &chunk.kind, &chunk.data)?;
    }
    Ok(())
}

pub(crate) fn export_character_png(
    prepared: PreparedRisuSaveExport,
    character_id: &str,
    metadata: Value,
    owned_directory: &Path,
    handoff_directory: &Path,
    destination_path: Option<&Path>,
    job: &JobControl,
) -> Result<JobResultSummary, NativeJobError> {
    export_character_png_with_release(
        prepared,
        character_id,
        metadata,
        owned_directory,
        handoff_directory,
        destination_path,
        job,
        PreparedRisuSaveExport::release_reader,
    )
}

#[allow(clippy::too_many_arguments)]
fn export_character_png_with_release<F>(
    mut prepared: PreparedRisuSaveExport,
    character_id: &str,
    mut metadata: Value,
    owned_directory: &Path,
    handoff_directory: &Path,
    destination_path: Option<&Path>,
    job: &JobControl,
    release_reader: F,
) -> Result<JobResultSummary, NativeJobError>
where
    F: FnOnce(&mut PreparedRisuSaveExport) -> StoreResult<()>,
{
    let mut release_reader = Some(release_reader);
    let outcome = (|| {
        if job.is_cancel_requested() {
            return Err(cancelled("PNG export cancelled before encoding"));
        }
        job.start(JobPhase::WritingExport).map_err(job_error)?;
        let repository =
            PayloadCas::new(prepared.repository_root().map_err(store_error)?).map_err(io_error)?;
        let (portrait, sources) = {
            let reader = prepared.reader().map_err(store_error)?;
            let projected = export::projected_character(
                &reader.connection,
                &prepared.snapshots_dir,
                &reader.target,
                character_id,
            )
            .map_err(store_error)?;
            character_json_export::validate_metadata(&projected.value, character_id, &metadata)?;
            let sources = character_json_export::json_asset_sources(
                &projected.value,
                projected.additional_asset_entries.as_deref(),
                &metadata,
                reader,
                &repository,
            )?;
            let portrait = portrait_bytes(&projected.value, reader, &repository, job)?;
            (portrait, sources)
        };
        let assets = metadata
            .pointer_mut("/data/assets")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| invalid_input("CCv3 assets must be an array"))?;
        let mut next_reference = 0_u64;
        let references = sources
            .iter()
            .enumerate()
            .map(|(index, source)| {
                let Some(_) = source else { return Ok(None) };
                next_reference = next_reference
                    .checked_add(1)
                    .ok_or_else(|| invalid_input("PNG asset reference overflow"))?;
                let reference = next_reference.to_string();
                assets[index]
                    .as_object_mut()
                    .ok_or_else(|| invalid_input("CCv3 asset must be an object"))?
                    .insert(
                        "uri".to_owned(),
                        Value::String(format!("__asset:{reference}")),
                    );
                Ok(Some(reference))
            })
            .collect::<Result<Vec<_>, NativeJobError>>()?;
        if next_reference > MAX_EMBEDDED_ASSET_COUNT {
            return Err(invalid_input("PNG embedded asset count exceeds its limit"));
        }
        let embedded_asset_bytes = sources.iter().flatten().try_fold(
            0_u64,
            |total, source| -> Result<u64, NativeJobError> {
                let size = match source {
                    JsonAssetSource::Cas { size, .. } => *size,
                    JsonAssetSource::FallbackPortrait => FALLBACK_PORTRAIT.len() as u64,
                };
                if size > MAX_EMBEDDED_ASSET_BYTES {
                    return Err(invalid_input(
                        "PNG embedded asset exceeds the importer limit",
                    ));
                }
                total
                    .checked_add(size)
                    .ok_or_else(|| invalid_input("PNG embedded asset total length overflow"))
            },
        )?;
        if embedded_asset_bytes > MAX_EMBEDDED_ASSET_TOTAL_BYTES {
            return Err(invalid_input("PNG embedded asset total exceeds its limit"));
        }
        let metadata = serde_json::to_vec(&metadata)
            .map_err(|error| invalid_input(format!("CCv3 metadata cannot be encoded: {error}")))?;
        if metadata.len() > 5 * 1024 * 1024 {
            return Err(invalid_input(
                "PNG card metadata exceeds the importer limit",
            ));
        }
        let portrait = prepare_png_portrait(&portrait, PngExportLimits::default(), || {
            job.is_cancel_requested()
        })?;
        let source = owned_directory.join("character.png");
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&source)
            .map_err(io_error)?;
        let mut output = BufWriter::new(file);
        let chunks = parse_chunks(&portrait)?;
        output.write_all(PNG_SIGNATURE).map_err(io_error)?;
        let mut completed_items = 0_u64;
        let total_items = next_reference;
        for chunk in chunks {
            if job.is_cancel_requested() {
                return Err(cancelled("PNG export cancelled while writing chunks"));
            }
            if chunk.kind == *b"IEND" {
                write_text_chunk(&mut output, "ccv3", &STANDARD.encode(&metadata))?;
                for (source, reference) in sources.iter().zip(&references) {
                    let (Some(source), Some(reference)) = (source, reference) else {
                        continue;
                    };
                    write_asset_text_chunk(&mut output, reference, source, &repository, job)?;
                    completed_items += 1;
                    job.set_progress(JobProgress {
                        completed_bytes: 0,
                        total_bytes: None,
                        completed_items,
                        total_items: Some(total_items),
                    })
                    .map_err(job_error)?;
                }
                write_chunk(&mut output, &chunk.kind, &chunk.data)?;
            } else if chunk.kind != *b"tEXt" || !owned_text_chunk(&chunk.data) {
                write_chunk(&mut output, &chunk.kind, &chunk.data)?;
            }
        }
        output.flush().map_err(io_error)?;
        output.get_ref().sync_all().map_err(io_error)?;
        drop(output);
        let _source_bytes = fs::metadata(&source).map_err(io_error)?.len();
        if job.is_cancel_requested() {
            return Err(cancelled(
                "PNG export cancelled before destination publication",
            ));
        }
        release_reader.take().ok_or_else(|| {
            NativeJobError::new("store-error", "revision release was already used")
        })?(&mut prepared)
        .map_err(store_error)?;
        job.set_phase(JobPhase::PublishingDestination)
            .map_err(job_error)?;
        let (destination_root, destination_path, handoff_path) = match destination_path {
            Some(destination_path) => (
                destination_path.parent().ok_or_else(|| {
                    NativeJobError::new(
                        "invalid-destination",
                        "PNG destination directory is unavailable",
                    )
                })?,
                destination_path.to_owned(),
                None,
            ),
            None => {
                fs::create_dir_all(handoff_directory).map_err(io_error)?;
                let path =
                    handoff_directory.join(format!("risu-character-card-{}.png", Uuid::new_v4()));
                (
                    handoff_directory,
                    path.clone(),
                    Some(path.to_string_lossy().into_owned()),
                )
            }
        };
        let phase_failure = RefCell::new(None);
        let published = destination::write_character_card_destination_controlled(
            owned_directory,
            &source,
            destination_root,
            &destination_path,
            || job.is_cancel_requested() || phase_failure.borrow().is_some(),
            |_| {},
            || {
                if job.is_cancel_requested() || phase_failure.borrow().is_some() {
                    return Err(destination::DestinationWriteError::Cancelled);
                }
                job.set_phase(JobPhase::FinalizingExport).map_err(|error| {
                    *phase_failure.borrow_mut() = Some(error);
                    destination::DestinationWriteError::Cancelled
                })
            },
        )
        .map_err(|error| {
            phase_failure
                .into_inner()
                .map(job_error)
                .unwrap_or_else(|| destination_error(error))
        })?;
        Ok(JobResultSummary {
            revision: prepared.revision,
            source_bytes: published.bytes,
            source_sha256: published.sha256,
            character_count: 1,
            preset_count: 0,
            warning_codes: Vec::new(),
            handoff_path,
            publication: None,
        })
    })();
    match release_reader.take() {
        Some(release_reader) => super::error::finish_with_release(
            outcome,
            release_reader(&mut prepared),
            "revision release failed",
        ),
        None => outcome,
    }
}

fn portrait_bytes(
    character: &Value,
    reader: &crate::persistent_store::RevisionReadLease,
    repository: &PayloadCas,
    job: &JobControl,
) -> Result<Vec<u8>, NativeJobError> {
    let Some(key) = character
        .get("image")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Ok(FALLBACK_PORTRAIT.to_vec());
    };
    let alias =
        export::pinned_asset_alias(&reader.connection, &reader.target, key).map_err(store_error)?;
    let hash = alias
        .object_hash
        .ok_or_else(|| invalid_input("pinned character portrait has no native payload"))?;
    let size = u64::try_from(alias.size)
        .map_err(|_| invalid_input("pinned character portrait size is invalid"))?;
    if size > PngExportLimits::default().max_input_bytes as u64 {
        return Err(invalid_input(
            "character portrait exceeds the PNG input limit",
        ));
    }
    read_verified_object(repository, &hash, size, job)
}

fn read_verified_object(
    repository: &PayloadCas,
    hash: &str,
    size: u64,
    job: &JobControl,
) -> Result<Vec<u8>, NativeJobError> {
    let mut source = repository
        .open_available_object(hash)
        .map_err(io_error)?
        .ok_or_else(|| invalid_input("pinned character asset payload is missing"))?;
    super::verified_read::read_verified_bytes(
        &mut source,
        hash,
        size,
        job,
        &super::verified_read::VerifiedReadLabels {
            cancelled: "PNG export cancelled while reading an asset",
            capacity: "asset size does not fit this platform",
            size_changed: "pinned character asset size changed",
            hash_changed: "pinned character asset payload hash changed",
        },
    )
}

fn write_asset_text_chunk(
    output: &mut impl Write,
    reference: &str,
    source: &JsonAssetSource,
    repository: &PayloadCas,
    job: &JobControl,
) -> Result<(), NativeJobError> {
    let keyword = format!("chara-ext-asset_:{reference}");
    let (size, expected_hash, mut input): (u64, Option<&str>, Box<dyn Read>) = match source {
        JsonAssetSource::Cas { hash, size, .. } => (
            *size,
            Some(hash),
            Box::new(
                repository
                    .open_available_object(hash)
                    .map_err(io_error)?
                    .ok_or_else(|| invalid_input("pinned character asset payload is missing"))?,
            ),
        ),
        JsonAssetSource::FallbackPortrait => (
            FALLBACK_PORTRAIT.len() as u64,
            None,
            Box::new(Cursor::new(FALLBACK_PORTRAIT)),
        ),
    };
    if size > MAX_EMBEDDED_ASSET_BYTES {
        return Err(invalid_input(
            "PNG embedded asset exceeds the importer limit",
        ));
    }
    let encoded = size
        .checked_add(2)
        .map(|value| value / 3)
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| invalid_input("PNG embedded asset length overflow"))?;
    let data_length = u64::try_from(keyword.len() + 1)
        .unwrap()
        .checked_add(encoded)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| invalid_input("PNG embedded asset chunk exceeds its limit"))?;
    output
        .write_all(&data_length.to_be_bytes())
        .map_err(io_error)?;
    output.write_all(b"tEXt").map_err(io_error)?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(b"tEXt");
    output.write_all(keyword.as_bytes()).map_err(io_error)?;
    output.write_all(&[0]).map_err(io_error)?;
    crc.update(keyword.as_bytes());
    crc.update(&[0]);
    let mut hasher = Sha256::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    {
        let mut verifier = HashingWriter {
            output,
            crc: &mut crc,
        };
        let mut encoder = EncoderWriter::new(&mut verifier, &STANDARD);
        loop {
            if job.is_cancel_requested() {
                return Err(cancelled("PNG export cancelled while writing an asset"));
            }
            let read = input.read(&mut buffer).map_err(io_error)?;
            if read == 0 {
                break;
            }
            copied = copied
                .checked_add(read as u64)
                .ok_or_else(|| invalid_input("PNG asset size overflow"))?;
            if copied > size {
                return Err(invalid_input("pinned character asset size changed"));
            }
            hasher.update(&buffer[..read]);
            encoder.write_all(&buffer[..read]).map_err(io_error)?;
        }
        encoder.finish().map_err(io_error)?;
    }
    if copied != size || expected_hash.is_some_and(|hash| hex::encode(hasher.finalize()) != hash) {
        return Err(invalid_input("pinned character asset payload hash changed"));
    }
    output
        .write_all(&crc.finalize().to_be_bytes())
        .map_err(io_error)
}

struct HashingWriter<'a, W> {
    output: &'a mut W,
    crc: &'a mut crc32fast::Hasher,
}

impl<W: Write> Write for HashingWriter<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self.output.write(buffer)?;
        self.crc.update(&buffer[..written]);
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

fn destination_error(error: destination::DestinationWriteError) -> NativeJobError {
    destination_error_with(
        error,
        "PNG source is invalid",
        "PNG destination is invalid",
        "PNG export cancelled before destination replacement",
    )
}

fn owned_text_chunk(data: &[u8]) -> bool {
    let keyword = data.split(|byte| *byte == 0).next().unwrap_or_default();
    keyword == b"chara" || keyword == b"ccv3" || keyword.starts_with(b"chara-ext-asset_")
}

fn write_text_chunk(
    output: &mut impl Write,
    keyword: &str,
    value: &str,
) -> Result<(), NativeJobError> {
    if keyword.is_empty() || keyword.as_bytes().len() > 79 || keyword.as_bytes().contains(&0) {
        return Err(invalid_input("PNG text keyword is invalid"));
    }
    let mut data = Vec::with_capacity(keyword.len() + 1 + value.len());
    data.extend_from_slice(keyword.as_bytes());
    data.push(0);
    data.extend_from_slice(value.as_bytes());
    write_chunk(output, b"tEXt", &data)
}

fn write_chunk(output: &mut impl Write, kind: &[u8; 4], data: &[u8]) -> Result<(), NativeJobError> {
    let length = u32::try_from(data.len()).map_err(|_| invalid_input("PNG chunk is too large"))?;
    output.write_all(&length.to_be_bytes()).map_err(io_error)?;
    output.write_all(kind).map_err(io_error)?;
    output.write_all(data).map_err(io_error)?;
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    output
        .write_all(&crc.finalize().to_be_bytes())
        .map_err(io_error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_repository::PayloadCas;
    use crate::import_export_jobs::png_card::{parse_png_card, PngCardLimits};
    use crate::native_file_jobs::{JobKind, JobRegistry};
    use crate::persistent_store::{AssetAlias, PersistentStore, StoreError};
    use base64::engine::general_purpose::STANDARD;
    use serde_json::json;
    use std::cell::Cell;
    use std::io::Cursor;
    use tempfile::TempDir;

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&(data.len() as u32).to_be_bytes());
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(data);
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        bytes.extend_from_slice(&crc.finalize().to_be_bytes());
        bytes
    }

    fn synthetic_png() -> Vec<u8> {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&[17, 34, 51, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        bytes
    }

    fn before_iend(mut png: Vec<u8>, additions: &[Vec<u8>]) -> Vec<u8> {
        let iend = png.len() - 12;
        let tail = png.split_off(iend);
        for addition in additions {
            png.extend_from_slice(addition);
        }
        png.extend_from_slice(&tail);
        png
    }

    #[test]
    fn replaces_only_owned_text_chunks_and_writes_valid_ordered_crcs() {
        let base = before_iend(
            synthetic_png(),
            &[
                chunk(b"tEXt", b"keep\0safe"),
                chunk(b"tEXt", b"chara\0old"),
                chunk(b"tEXt", b"ccv3\0old"),
                chunk(b"tEXt", b"chara-ext-asset_:old\0b2xk"),
            ],
        );
        let metadata = br#"{"spec":"chara_card_v3","spec_version":"3.0","data":{"name":"Card"}}"#;
        let assets = vec![
            ("7".to_owned(), b"first".to_vec()),
            ("8".to_owned(), Vec::new()),
        ];

        let mut output = Vec::new();
        write_png_card(
            &mut Cursor::new(base),
            &mut output,
            metadata,
            &assets,
            || false,
        )
        .unwrap();

        let parsed = parse_chunks(&output).unwrap();
        let texts = parsed
            .iter()
            .filter(|chunk| chunk.kind == *b"tEXt")
            .map(|chunk| chunk.data.clone())
            .collect::<Vec<_>>();
        assert_eq!(texts[0], b"keep\0safe");
        assert_eq!(
            texts[1],
            [b"ccv3\0".as_slice(), STANDARD.encode(metadata).as_bytes()].concat()
        );
        assert_eq!(
            texts[2],
            [
                b"chara-ext-asset_:7\0".as_slice(),
                STANDARD.encode(b"first").as_bytes()
            ]
            .concat()
        );
        assert_eq!(texts[3], b"chara-ext-asset_:8\0");
        assert_eq!(parsed.last().unwrap().kind, *b"IEND");
    }

    #[test]
    fn boundedly_reencodes_a_non_png_portrait() {
        let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([9, 8, 7, 255]));
        let mut webp = Vec::new();
        image::DynamicImage::ImageRgba8(image)
            .write_to(&mut Cursor::new(&mut webp), image::ImageFormat::WebP)
            .unwrap();
        let png = prepare_png_portrait(&webp, PngExportLimits::default(), || false).unwrap();
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(parse_chunks(&png).is_ok());
    }

    fn alias(key: &str, bytes: &[u8], ext: &str, cas: &PayloadCas) -> AssetAlias {
        let payload = cas.prepare_bytes(bytes).unwrap();
        AssetAlias {
            key: key.to_owned(),
            object_hash: Some(payload.content_hash),
            kind: "asset".to_owned(),
            size: payload.byte_size as i64,
            mime: "image/png".to_owned(),
            name: key.to_owned(),
            ext: ext.to_owned(),
            inlay_type: None,
            width: None,
            height: None,
            metadata: json!({}),
        }
    }

    fn publication_fixture() -> (TempDir, PersistentStore, i64, Value) {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let portrait = alias("assets/portrait.png", &synthetic_png(), "png", &cas);
        let character = json!({
            "type": "character", "chaId": "png-publication", "name": "PNG Card",
            "image": portrait.key, "ccAssets": [], "additionalAssets": [],
            "emotionImages": [], "triggerscript": [], "customscript": [], "chats": []
        });
        let staging = store.replace_begin().unwrap().staging_id;
        store.replace_put_root(&staging, &json!({})).unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        store
            .replace_add_characters(&staging, &[character])
            .unwrap();
        store
            .replace_put_asset_aliases(&staging, &[portrait])
            .unwrap();
        let revision = store.replace_commit(&staging, Some(0)).unwrap().revision;
        let metadata = json!({
            "spec": "chara_card_v3", "spec_version": "3.0",
            "data": {
                "name": "PNG Card",
                "extensions": {"risuai": {"triggerscript": [], "customScripts": []}},
                "assets": [{"type": "icon", "uri": "ccdefault:", "name": "main", "ext": "png"}]
            }
        });
        (directory, store, revision, metadata)
    }

    #[test]
    fn png_release_failure_prevents_desktop_and_handoff_publication_exactly_once() {
        for portable in [false, true] {
            let (directory, mut store, revision, metadata) = publication_fixture();
            let prepared = store.prepare_risu_save_export(revision).unwrap();
            let owned = directory.path().join("owned");
            let chosen = directory.path().join("chosen");
            let handoffs = directory.path().join("handoffs");
            fs::create_dir(&owned).unwrap();
            fs::create_dir(&chosen).unwrap();
            let destination = chosen.join("card.png");
            fs::write(&destination, b"previous PNG").unwrap();
            let job = JobRegistry::default()
                .create(JobKind::ExportCharacterCard)
                .unwrap();
            let release_calls = Cell::new(0);

            let error = export_character_png_with_release(
                prepared,
                "png-publication",
                metadata,
                &owned,
                &handoffs,
                (!portable).then_some(destination.as_path()),
                &job,
                |prepared| {
                    release_calls.set(release_calls.get() + 1);
                    prepared.release_reader()?;
                    Err(StoreError::Store {
                        message: "injected checkpoint failure".to_owned(),
                    })
                },
            )
            .unwrap_err();

            assert_eq!(error.code, "store-error");
            assert_eq!(release_calls.get(), 1);
            assert_eq!(fs::read(&destination).unwrap(), b"previous PNG");
            assert!(!handoffs.exists());
        }
    }

    #[test]
    fn png_cancellation_and_stale_revision_do_not_publish() {
        let (directory, mut store, revision, metadata) = publication_fixture();
        assert!(matches!(
            store.prepare_risu_save_export(revision + 1).err().unwrap(),
            StoreError::RevisionConflict { .. }
        ));
        let prepared = store.prepare_risu_save_export(revision).unwrap();
        let owned = directory.path().join("owned");
        let chosen = directory.path().join("chosen");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&chosen).unwrap();
        let destination = chosen.join("card.png");
        fs::write(&destination, b"previous PNG").unwrap();
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCard)
            .unwrap();
        job.request_cancel().unwrap();

        let error = export_character_png(
            prepared,
            "png-publication",
            metadata,
            &owned,
            &directory.path().join("handoffs"),
            Some(&destination),
            &job,
        )
        .unwrap_err();
        assert_eq!(error.code, "cancelled");
        assert_eq!(fs::read(destination).unwrap(), b"previous PNG");
    }

    #[test]
    fn native_png_writer_roundtrips_exact_metadata_and_payload_hashes() {
        let directory = TempDir::new().unwrap();
        let mut store = PersistentStore::open(directory.path()).unwrap();
        let cas = PayloadCas::new(directory.path()).unwrap();
        let portrait_bytes = synthetic_png();
        let portrait = alias("assets/portrait.png", &portrait_bytes, "png", &cas);
        let asset = alias("assets/exact.bin", b"exact embedded payload", "bin", &cas);
        let character = json!({
            "type": "character", "chaId": "png-character", "name": "PNG Card",
            "image": portrait.key, "ccAssets": [],
            "additionalAssets": [["exact", asset.key, "bin"]],
            "emotionImages": [], "triggerscript": [], "customscript": [], "chats": []
        });
        let staging = store.replace_begin().unwrap().staging_id;
        store.replace_put_root(&staging, &json!({})).unwrap();
        store.replace_put_presets(&staging, &[]).unwrap();
        store
            .replace_add_characters(&staging, &[character])
            .unwrap();
        store
            .replace_put_asset_aliases(&staging, &[portrait, asset.clone()])
            .unwrap();
        let revision = store.replace_commit(&staging, Some(0)).unwrap().revision;
        let metadata = json!({
            "spec": "chara_card_v3", "spec_version": "3.0",
            "data": {
                "name": "PNG Card",
                "extensions": {"risuai": {"triggerscript": [], "customScripts": []}},
                "assets": [
                    {"type": "x-risu-asset", "uri": asset.key, "name": "exact", "ext": "bin"},
                    {"type": "icon", "uri": "ccdefault:", "name": "main", "ext": "png"}
                ]
            }
        });
        let prepared = store.prepare_risu_save_export(revision).unwrap();
        let owned = directory.path().join("owned");
        let chosen = directory.path().join("chosen");
        fs::create_dir(&owned).unwrap();
        fs::create_dir(&chosen).unwrap();
        let destination = chosen.join("card.png");
        let job = JobRegistry::default()
            .create(JobKind::ExportCharacterCard)
            .unwrap();
        export_character_png(
            prepared,
            "png-character",
            metadata,
            &owned,
            &directory.path().join("handoffs"),
            Some(&destination),
            &job,
        )
        .unwrap();
        let parse_root = directory.path().join("parse");
        fs::create_dir(&parse_root).unwrap();
        let parsed = parse_png_card(
            &mut fs::File::open(destination).unwrap(),
            &parse_root,
            PngCardLimits::default(),
            || false,
        )
        .unwrap();
        assert_eq!(
            parsed.base_image.sha256,
            hex::encode(Sha256::digest(&portrait_bytes))
        );
        assert_eq!(parsed.embedded_assets.len(), 2);
        assert_eq!(parsed.embedded_assets[0].sha256, asset.object_hash.unwrap());
        let card = STANDARD.decode(parsed.ccv3.unwrap()).unwrap();
        let card: Value = serde_json::from_slice(&card).unwrap();
        assert_eq!(card["data"]["assets"][0]["uri"], "__asset:1");
        assert_eq!(card["data"]["assets"][1]["uri"], "__asset:2");
    }
}
