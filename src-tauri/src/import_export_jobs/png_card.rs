use crate::trust_boundary::is_link_like;
use base64::{engine::general_purpose::STANDARD, read::DecoderReader, Engine as _};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    path::{Path, PathBuf},
};

const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const COPY_BUFFER_BYTES: usize = 64 * 1024;
const MAX_PNG_CHUNK_BYTES: u64 = i32::MAX as u64;
const MAX_TEXT_KEYWORD_BYTES: usize = 79;

#[derive(Debug, Clone, Copy)]
pub(crate) struct PngCardLimits {
    pub max_card_metadata_bytes: u64,
    pub max_recognized_metadata_bytes: u64,
    pub max_embedded_asset_bytes: u64,
    pub max_embedded_asset_total_bytes: u64,
    pub max_embedded_asset_count: usize,
}

impl Default for PngCardLimits {
    fn default() -> Self {
        Self {
            max_card_metadata_bytes: crate::import_export_jobs::MAX_CONTENT_METADATA_BYTES as u64,
            max_recognized_metadata_bytes: 768 * 1024 * 1024,
            max_embedded_asset_bytes: 50 * 1024 * 1024,
            max_embedded_asset_total_bytes: 512 * 1024 * 1024,
            max_embedded_asset_count: 10_000,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StagedPngPayload {
    pub staged_id: String,
    pub byte_size: u64,
    pub sha256: String,
    pub source_chunk_keyword: Option<String>,
    pub asset_reference: Option<String>,
    pub source_extension: Option<String>,
    pub source_media_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PngCardParseResult {
    pub chara: Option<String>,
    pub ccv3: Option<String>,
    pub base_image: StagedPngPayload,
    pub embedded_assets: Vec<StagedPngPayload>,
}

#[derive(Debug)]
pub(crate) enum PngCardError {
    Io(io::Error),
    Invalid(String),
    LimitExceeded(String),
    Cancelled,
}

impl fmt::Display for PngCardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::Invalid(message) | Self::LimitExceeded(message) => formatter.write_str(message),
            Self::Cancelled => formatter.write_str("PNG card parsing was cancelled"),
        }
    }
}

impl std::error::Error for PngCardError {}

impl From<io::Error> for PngCardError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

struct OwnedStagedPayload {
    file: Option<File>,
    path: PathBuf,
    descriptor: StagedPngPayload,
    hasher: Option<Sha256>,
    owned: bool,
}

impl OwnedStagedPayload {
    fn create(
        staging_directory: &Path,
        suffix: &str,
        source_chunk_keyword: Option<String>,
        asset_reference: Option<String>,
        source_extension: Option<String>,
        source_media_type: Option<String>,
    ) -> Result<Self, PngCardError> {
        let staged_id = format!("{}.{suffix}.stage", uuid::Uuid::new_v4());
        let path = staging_directory.join(&staged_id);
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            file: Some(file),
            path,
            descriptor: StagedPngPayload {
                staged_id,
                byte_size: 0,
                sha256: String::new(),
                source_chunk_keyword,
                asset_reference,
                source_extension,
                source_media_type,
            },
            hasher: Some(Sha256::new()),
            owned: true,
        })
    }

    fn finish(&mut self) -> Result<(), PngCardError> {
        let mut file = self.file.take().ok_or_else(|| {
            PngCardError::Invalid("staged payload was already finished".to_string())
        })?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        self.descriptor.sha256 = hex::encode(
            self.hasher
                .take()
                .ok_or_else(|| {
                    PngCardError::Invalid("staged payload hash was already finalized".to_string())
                })?
                .finalize(),
        );
        Ok(())
    }

    fn into_descriptor(mut self) -> StagedPngPayload {
        self.owned = false;
        self.descriptor.clone()
    }
}

impl Write for OwnedStagedPayload {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let written = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::new(ErrorKind::BrokenPipe, "staged payload is closed"))?
            .write(buffer)?;
        self.hasher
            .as_mut()
            .ok_or_else(|| io::Error::new(ErrorKind::BrokenPipe, "staged payload hash is closed"))?
            .update(&buffer[..written]);
        self.descriptor.byte_size = self
            .descriptor
            .byte_size
            .checked_add(written as u64)
            .ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidData, "staged payload size overflow")
            })?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .ok_or_else(|| io::Error::new(ErrorKind::BrokenPipe, "staged payload is closed"))?
            .flush()
    }
}

impl Drop for OwnedStagedPayload {
    fn drop(&mut self) {
        if self.owned {
            self.file.take();
            let _ = fs::remove_file(&self.path);
        }
    }
}

struct ChunkReader<'a, R, F> {
    source: &'a mut R,
    remaining: u64,
    crc: &'a mut crc32fast::Hasher,
    is_cancelled: &'a F,
}

impl<R: Read, F: Fn() -> bool> Read for ChunkReader<'_, R, F> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if (self.is_cancelled)() {
            return Err(io::Error::new(ErrorKind::Interrupted, "cancelled"));
        }
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let limit = usize::try_from(self.remaining.min(buffer.len() as u64))
            .expect("bounded by buffer length");
        let read = self.source.read(&mut buffer[..limit])?;
        if read == 0 {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "truncated PNG chunk data",
            ));
        }
        self.crc.update(&buffer[..read]);
        self.remaining -= read as u64;
        Ok(read)
    }
}

pub(crate) fn parse_png_card<R: Read, F: Fn() -> bool>(
    source: &mut R,
    staging_directory: &Path,
    limits: PngCardLimits,
    is_cancelled: F,
) -> Result<PngCardParseResult, PngCardError> {
    let staging_directory = validate_staging_directory(staging_directory)?;
    check_cancelled(&is_cancelled)?;

    let mut signature = [0_u8; PNG_SIGNATURE.len()];
    read_exact_cancelled(source, &mut signature, &is_cancelled, "PNG signature")?;
    if &signature != PNG_SIGNATURE {
        return Err(PngCardError::Invalid("invalid PNG signature".to_string()));
    }

    let mut base_image = OwnedStagedPayload::create(
        &staging_directory,
        "png",
        None,
        None,
        Some("png".to_string()),
        Some("image/png".to_string()),
    )?;
    base_image.write_all(PNG_SIGNATURE)?;
    let mut embedded_assets = Vec::<OwnedStagedPayload>::new();
    let mut asset_by_reference = HashMap::<String, usize>::new();
    let mut chara = None;
    let mut ccv3 = None;
    let mut recognized_metadata_bytes = 0_u64;
    let mut embedded_asset_total_bytes = 0_u64;
    let mut source_offset = PNG_SIGNATURE.len() as u64;
    let mut chunk_index = 0_u64;
    let mut seen_ihdr = false;
    let mut seen_idat = false;

    loop {
        check_cancelled(&is_cancelled)?;
        let mut length_bytes = [0_u8; 4];
        read_exact_cancelled(source, &mut length_bytes, &is_cancelled, "PNG chunk length")?;
        let data_length = u32::from_be_bytes(length_bytes) as u64;
        if data_length > MAX_PNG_CHUNK_BYTES {
            return Err(PngCardError::Invalid(
                "PNG chunk length exceeds the PNG limit".to_string(),
            ));
        }
        let mut chunk_type = [0_u8; 4];
        read_exact_cancelled(source, &mut chunk_type, &is_cancelled, "PNG chunk type")?;
        if !chunk_type.iter().all(u8::is_ascii_alphabetic) {
            return Err(PngCardError::Invalid(
                "PNG chunk type contains non-letter bytes".to_string(),
            ));
        }
        source_offset = source_offset
            .checked_add(12)
            .and_then(|offset| offset.checked_add(data_length))
            .ok_or_else(|| PngCardError::Invalid("PNG source offset overflow".to_string()))?;

        if chunk_index == 0 && &chunk_type != b"IHDR" {
            return Err(PngCardError::Invalid(
                "PNG IHDR must be the first chunk".to_string(),
            ));
        }
        if &chunk_type == b"IHDR" {
            if seen_ihdr || data_length != 13 {
                return Err(PngCardError::Invalid("invalid PNG IHDR chunk".to_string()));
            }
            seen_ihdr = true;
        }
        if &chunk_type == b"IDAT" {
            seen_idat = true;
        }
        if &chunk_type == b"IEND" && data_length != 0 {
            return Err(PngCardError::Invalid("invalid PNG IEND length".to_string()));
        }

        let mut crc = crc32fast::Hasher::new();
        crc.update(&chunk_type);
        let is_card_text = if &chunk_type == b"tEXt" {
            parse_text_chunk(
                source,
                data_length,
                &length_bytes,
                &chunk_type,
                &mut crc,
                &mut base_image,
                &staging_directory,
                limits,
                &mut recognized_metadata_bytes,
                &mut embedded_asset_total_bytes,
                &mut chara,
                &mut ccv3,
                &mut embedded_assets,
                &mut asset_by_reference,
                &is_cancelled,
            )?
        } else {
            base_image.write_all(&length_bytes)?;
            base_image.write_all(&chunk_type)?;
            let mut chunk_reader = ChunkReader {
                source,
                remaining: data_length,
                crc: &mut crc,
                is_cancelled: &is_cancelled,
            };
            copy_all(&mut chunk_reader, &mut base_image, &is_cancelled)?;
            false
        };

        let mut expected_crc_bytes = [0_u8; 4];
        read_exact_cancelled(
            source,
            &mut expected_crc_bytes,
            &is_cancelled,
            "PNG chunk CRC",
        )?;
        let expected_crc = u32::from_be_bytes(expected_crc_bytes);
        let actual_crc = crc.finalize();
        if expected_crc != actual_crc {
            return Err(PngCardError::Invalid(format!(
                "PNG chunk CRC mismatch for {}",
                String::from_utf8_lossy(&chunk_type)
            )));
        }
        if !is_card_text {
            base_image.write_all(&expected_crc_bytes)?;
        }

        chunk_index = chunk_index
            .checked_add(1)
            .ok_or_else(|| PngCardError::Invalid("PNG chunk count overflow".to_string()))?;
        if &chunk_type == b"IEND" {
            let mut trailing = [0_u8; 1];
            check_cancelled(&is_cancelled)?;
            if source.read(&mut trailing)? != 0 {
                return Err(PngCardError::Invalid(
                    "PNG contains data after IEND".to_string(),
                ));
            }
            break;
        }
    }

    if !seen_ihdr || !seen_idat {
        return Err(PngCardError::Invalid(
            "PNG is missing required image chunks".to_string(),
        ));
    }
    let selected_card = ccv3
        .as_deref()
        .or(chara.as_deref())
        .ok_or_else(|| PngCardError::Invalid("PNG does not contain card metadata".to_string()))?;
    let selected_card = std::str::from_utf8(selected_card)
        .map_err(|_| PngCardError::Invalid("PNG card metadata is not UTF-8".to_string()))?;
    validate_card_metadata(selected_card, limits.max_card_metadata_bytes)?;

    let chara = chara.map(|value| String::from_utf8_lossy(&value).into_owned());
    let ccv3 = ccv3.map(|value| String::from_utf8_lossy(&value).into_owned());

    base_image.finish()?;
    let base_image = base_image.into_descriptor();
    let embedded_assets = embedded_assets
        .into_iter()
        .map(OwnedStagedPayload::into_descriptor)
        .collect();
    Ok(PngCardParseResult {
        chara,
        ccv3,
        base_image,
        embedded_assets,
    })
}

#[allow(clippy::too_many_arguments)]
fn parse_text_chunk<R: Read, F: Fn() -> bool>(
    source: &mut R,
    data_length: u64,
    length_bytes: &[u8; 4],
    chunk_type: &[u8; 4],
    crc: &mut crc32fast::Hasher,
    base_image: &mut OwnedStagedPayload,
    staging_directory: &Path,
    limits: PngCardLimits,
    recognized_metadata_bytes: &mut u64,
    embedded_asset_total_bytes: &mut u64,
    chara: &mut Option<Vec<u8>>,
    ccv3: &mut Option<Vec<u8>>,
    embedded_assets: &mut Vec<OwnedStagedPayload>,
    asset_by_reference: &mut HashMap<String, usize>,
    is_cancelled: &F,
) -> Result<bool, PngCardError> {
    if data_length == 0 {
        return Err(PngCardError::Invalid("empty PNG tEXt chunk".to_string()));
    }
    let mut keyword_bytes = Vec::with_capacity(MAX_TEXT_KEYWORD_BYTES + 1);
    let max_keyword_and_separator = data_length.min((MAX_TEXT_KEYWORD_BYTES + 1) as u64);
    while (keyword_bytes.len() as u64) < max_keyword_and_separator {
        let mut byte = [0_u8; 1];
        read_exact_cancelled(source, &mut byte, is_cancelled, "PNG tEXt keyword")?;
        crc.update(&byte);
        keyword_bytes.push(byte[0]);
        if byte[0] == 0 {
            break;
        }
    }
    if keyword_bytes.last() != Some(&0) || keyword_bytes.len() == 1 {
        return Err(PngCardError::Invalid(
            "invalid PNG tEXt keyword".to_string(),
        ));
    }
    let keyword = &keyword_bytes[..keyword_bytes.len() - 1];
    let value_length = data_length
        .checked_sub(keyword_bytes.len() as u64)
        .ok_or_else(|| PngCardError::Invalid("PNG tEXt length underflow".to_string()))?;
    let recognized_asset_keyword = std::str::from_utf8(keyword)
        .ok()
        .filter(|keyword| keyword.starts_with("chara-ext-asset_"));
    let recognized =
        keyword == b"chara" || keyword == b"ccv3" || recognized_asset_keyword.is_some();

    if !recognized {
        base_image.write_all(length_bytes)?;
        base_image.write_all(chunk_type)?;
        base_image.write_all(&keyword_bytes)?;
        let mut chunk_reader = ChunkReader {
            source,
            remaining: value_length,
            crc,
            is_cancelled,
        };
        copy_all(&mut chunk_reader, base_image, is_cancelled)?;
        return Ok(false);
    }

    *recognized_metadata_bytes = recognized_metadata_bytes
        .checked_add(data_length)
        .ok_or_else(|| PngCardError::LimitExceeded("PNG metadata size overflow".to_string()))?;
    if *recognized_metadata_bytes > limits.max_recognized_metadata_bytes {
        return Err(PngCardError::LimitExceeded(
            "PNG recognized metadata exceeds the aggregate limit".to_string(),
        ));
    }

    if keyword == b"chara" || keyword == b"ccv3" {
        if value_length > limits.max_card_metadata_bytes {
            return Err(PngCardError::LimitExceeded(
                "PNG card metadata exceeds the per-chunk limit".to_string(),
            ));
        }
        let capacity = usize::try_from(value_length).map_err(|_| {
            PngCardError::LimitExceeded("PNG card metadata cannot fit in memory".to_string())
        })?;
        let mut value = Vec::with_capacity(capacity);
        let mut chunk_reader = ChunkReader {
            source,
            remaining: value_length,
            crc,
            is_cancelled,
        };
        copy_all(&mut chunk_reader, &mut value, is_cancelled)?;
        let slot = if keyword == b"chara" { chara } else { ccv3 };
        match slot {
            Some(existing) if existing != &value => {
                return Err(PngCardError::Invalid(format!(
                    "conflicting duplicate {} card metadata",
                    String::from_utf8_lossy(keyword)
                )));
            }
            Some(_) => {}
            None => *slot = Some(value),
        }
        return Ok(true);
    }

    if embedded_assets.len() >= limits.max_embedded_asset_count {
        return Err(PngCardError::LimitExceeded(
            "PNG embedded asset count exceeds the limit".to_string(),
        ));
    }
    if value_length > maximum_base64_bytes(limits.max_embedded_asset_bytes)? {
        return Err(PngCardError::LimitExceeded(
            "PNG embedded asset exceeds the encoded size limit".to_string(),
        ));
    }
    let keyword = recognized_asset_keyword
        .expect("embedded asset keyword was classified as UTF-8")
        .to_string();
    let asset_reference = keyword
        .strip_prefix("chara-ext-asset_:")
        .or_else(|| keyword.strip_prefix("chara-ext-asset_"))
        .unwrap_or_default()
        .to_string();
    let mut staged = OwnedStagedPayload::create(
        staging_directory,
        "asset",
        Some(keyword),
        Some(asset_reference.clone()),
        None,
        None,
    )?;
    let mut chunk_reader = ChunkReader {
        source,
        remaining: value_length,
        crc,
        is_cancelled,
    };
    {
        let mut decoder = DecoderReader::new(&mut chunk_reader, &STANDARD);
        copy_decoded_asset(
            &mut decoder,
            &mut staged,
            limits.max_embedded_asset_bytes,
            limits.max_embedded_asset_total_bytes,
            embedded_asset_total_bytes,
            is_cancelled,
        )?;
    }
    if chunk_reader.remaining != 0 {
        return Err(PngCardError::Invalid(
            "invalid base64 in PNG embedded asset".to_string(),
        ));
    }
    staged.finish()?;

    if let Some(existing_index) = asset_by_reference.get(&asset_reference).copied() {
        if embedded_assets[existing_index].descriptor.sha256 != staged.descriptor.sha256
            || embedded_assets[existing_index].descriptor.byte_size != staged.descriptor.byte_size
        {
            return Err(PngCardError::Invalid(format!(
                "conflicting duplicate PNG embedded asset {asset_reference}"
            )));
        }
        return Ok(true);
    }
    asset_by_reference.insert(asset_reference, embedded_assets.len());
    embedded_assets.push(staged);
    Ok(true)
}

fn validate_card_metadata(value: &str, decoded_limit: u64) -> Result<(), PngCardError> {
    if value.starts_with("rcc||") {
        let parts: Vec<&str> = value.split("||").collect();
        if parts.len() != 5 || parts[1] != "rccv1" {
            return Err(PngCardError::Invalid(
                "invalid encrypted PNG card envelope".to_string(),
            ));
        }
        decode_bounded_base64(parts[2], decoded_limit, "encrypted PNG card payload")?;
        let metadata =
            decode_bounded_base64(parts[4], decoded_limit, "PNG card envelope metadata")?;
        let metadata = std::str::from_utf8(&metadata).map_err(|_| {
            PngCardError::Invalid("PNG card envelope metadata is not UTF-8".to_string())
        })?;
        serde_json::from_str::<serde_json::Value>(metadata).map_err(|_| {
            PngCardError::Invalid("PNG card envelope metadata is not valid JSON".to_string())
        })?;
        return Ok(());
    }

    let decoded = decode_bounded_base64(value, decoded_limit, "PNG card metadata")?;
    let decoded = std::str::from_utf8(&decoded)
        .map_err(|_| PngCardError::Invalid("decoded PNG card metadata is not UTF-8".to_string()))?;
    serde_json::from_str::<serde_json::Value>(decoded)
        .map_err(|_| PngCardError::Invalid("PNG card metadata is not valid JSON".to_string()))?;
    Ok(())
}

fn decode_bounded_base64(
    encoded: &str,
    decoded_limit: u64,
    label: &str,
) -> Result<Vec<u8>, PngCardError> {
    if encoded.len() as u64 > maximum_base64_bytes(decoded_limit)? {
        return Err(PngCardError::LimitExceeded(format!(
            "{label} exceeds the encoded size limit"
        )));
    }
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|_| PngCardError::Invalid(format!("invalid base64 in {label}")))?;
    if decoded.len() as u64 > decoded_limit {
        return Err(PngCardError::LimitExceeded(format!(
            "{label} exceeds the decoded size limit"
        )));
    }
    Ok(decoded)
}

fn maximum_base64_bytes(decoded_limit: u64) -> Result<u64, PngCardError> {
    decoded_limit
        .checked_add(2)
        .and_then(|value| value.checked_div(3))
        .and_then(|value| value.checked_mul(4))
        .ok_or_else(|| PngCardError::LimitExceeded("base64 size limit overflow".to_string()))
}

fn copy_decoded_asset<R: Read, F: Fn() -> bool>(
    source: &mut R,
    destination: &mut OwnedStagedPayload,
    per_asset_limit: u64,
    aggregate_limit: u64,
    aggregate_bytes: &mut u64,
    is_cancelled: &F,
) -> Result<(), PngCardError> {
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        check_cancelled(is_cancelled)?;
        let read = match source.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted && is_cancelled() => {
                return Err(PngCardError::Cancelled);
            }
            Err(error) if error.kind() == ErrorKind::InvalidData => {
                return Err(PngCardError::Invalid(
                    "invalid base64 in PNG embedded asset".to_string(),
                ));
            }
            Err(error) => return Err(PngCardError::Io(error)),
        };
        if read == 0 {
            break;
        }
        let next_asset_size = destination
            .descriptor
            .byte_size
            .checked_add(read as u64)
            .ok_or_else(|| PngCardError::LimitExceeded("PNG asset size overflow".to_string()))?;
        let next_aggregate = aggregate_bytes
            .checked_add(read as u64)
            .ok_or_else(|| PngCardError::LimitExceeded("PNG asset total overflow".to_string()))?;
        if next_asset_size > per_asset_limit {
            return Err(PngCardError::LimitExceeded(
                "PNG embedded asset exceeds the decoded size limit".to_string(),
            ));
        }
        if next_aggregate > aggregate_limit {
            return Err(PngCardError::LimitExceeded(
                "PNG embedded assets exceed the aggregate decoded size limit".to_string(),
            ));
        }
        destination.write_all(&buffer[..read])?;
        *aggregate_bytes = next_aggregate;
    }
    Ok(())
}

fn copy_all<R: Read, W: Write, F: Fn() -> bool>(
    source: &mut R,
    destination: &mut W,
    is_cancelled: &F,
) -> Result<(), PngCardError> {
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        check_cancelled(is_cancelled)?;
        let read = match source.read(&mut buffer) {
            Ok(read) => read,
            Err(error) if error.kind() == ErrorKind::Interrupted && is_cancelled() => {
                return Err(PngCardError::Cancelled);
            }
            Err(error) => return Err(PngCardError::Io(error)),
        };
        if read == 0 {
            return Ok(());
        }
        destination.write_all(&buffer[..read])?;
    }
}

fn read_exact_cancelled<R: Read, F: Fn() -> bool>(
    source: &mut R,
    mut destination: &mut [u8],
    is_cancelled: &F,
    label: &str,
) -> Result<(), PngCardError> {
    while !destination.is_empty() {
        check_cancelled(is_cancelled)?;
        let read = source.read(destination)?;
        if read == 0 {
            return Err(PngCardError::Invalid(format!("truncated {label}")));
        }
        destination = &mut destination[read..];
    }
    Ok(())
}

fn check_cancelled(is_cancelled: &impl Fn() -> bool) -> Result<(), PngCardError> {
    if is_cancelled() {
        Err(PngCardError::Cancelled)
    } else {
        Ok(())
    }
}

fn validate_staging_directory(path: &Path) -> Result<PathBuf, PngCardError> {
    let metadata = fs::symlink_metadata(path)?;
    if is_link_like(&metadata) || !metadata.is_dir() {
        return Err(PngCardError::Invalid(
            "PNG staging path is not an owned directory".to_string(),
        ));
    }
    Ok(fs::canonicalize(path)?)
}

#[cfg(test)]
mod tests {
    use super::{parse_png_card, PngCardError, PngCardLimits};
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use sha2::{Digest, Sha256};
    use std::{
        cell::Cell,
        io::{self, Cursor, Read},
    };

    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

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

    fn png_with(chunks: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        for chunk in chunks {
            bytes.extend(chunk);
        }
        bytes
    }

    fn ihdr() -> Vec<u8> {
        chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0])
    }

    fn text_chunk(keyword: &str, value: &str) -> Vec<u8> {
        text_chunk_bytes(keyword, value.as_bytes())
    }

    fn text_chunk_bytes(keyword: &str, value: &[u8]) -> Vec<u8> {
        chunk(b"tEXt", &[keyword.as_bytes(), b"\0", value].concat())
    }

    fn encoded_card(name: &str) -> String {
        STANDARD.encode(format!(
            r#"{{"spec":"chara_card_v2","data":{{"name":"{name}"}}}}"#
        ))
    }

    fn card_png(extra_chunks: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
        let mut chunks = vec![ihdr(), chunk(b"IDAT", &[1, 2, 3, 4])];
        chunks.extend(extra_chunks);
        chunks.push(text_chunk("chara", &encoded_card("Risu")));
        chunks.push(chunk(b"IEND", &[]));
        png_with(chunks)
    }

    fn assert_failure_cleans_staging(input: Vec<u8>, limits: PngCardLimits) -> PngCardError {
        let staging = tempfile::tempdir().expect("staging directory");
        let error = parse_png_card(&mut Cursor::new(input), staging.path(), limits, || false)
            .expect_err("PNG parsing must fail");
        assert_eq!(
            std::fs::read_dir(staging.path())
                .expect("read staging directory")
                .count(),
            0,
            "failed parsing must remove every owned staging file"
        );
        error
    }

    struct TinyReader<R> {
        inner: R,
        max_read: usize,
    }

    impl<R: Read> Read for TinyReader<R> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let limit = buffer.len().min(self.max_read);
            self.inner.read(&mut buffer[..limit])
        }
    }

    #[test]
    fn stages_an_exact_base_png_and_returns_bounded_card_metadata() {
        let metadata = r#"{"spec":"chara_card_v2","data":{"name":"Risu"}}"#;
        let encoded = STANDARD.encode(metadata);
        let text = [b"chara\0".as_slice(), encoded.as_bytes()].concat();
        let idat = chunk(b"IDAT", &[1, 2, 3, 4]);
        let iend = chunk(b"IEND", &[]);
        let input = png_with([ihdr(), chunk(b"tEXt", &text), idat.clone(), iend.clone()]);
        let expected_base = png_with([ihdr(), idat, iend]);
        let staging = tempfile::tempdir().expect("staging directory");

        let result = parse_png_card(
            &mut Cursor::new(input),
            staging.path(),
            PngCardLimits::default(),
            || false,
        )
        .expect("parse PNG card");

        assert_eq!(result.chara.as_deref(), Some(encoded.as_str()));
        assert_eq!(result.ccv3, None);
        assert_eq!(result.embedded_assets, Vec::new());
        assert_eq!(result.base_image.source_extension.as_deref(), Some("png"));
        assert_eq!(
            result.base_image.source_media_type.as_deref(),
            Some("image/png")
        );
        assert_eq!(
            result.base_image.sha256,
            hex::encode(Sha256::digest(&expected_base))
        );
        assert_eq!(
            std::fs::read(staging.path().join(&result.base_image.staged_id))
                .expect("read staged base image"),
            expected_base
        );
    }

    #[test]
    fn parses_across_one_byte_reads_and_preserves_unknown_text_chunks() {
        let unknown_text = text_chunk("Comment", "kept byte for byte");
        let input = card_png([unknown_text.clone()]);
        let expected_base = png_with([
            ihdr(),
            chunk(b"IDAT", &[1, 2, 3, 4]),
            unknown_text,
            chunk(b"IEND", &[]),
        ]);
        let staging = tempfile::tempdir().expect("staging directory");
        let mut reader = TinyReader {
            inner: Cursor::new(input),
            max_read: 1,
        };

        let result = parse_png_card(
            &mut reader,
            staging.path(),
            PngCardLimits::default(),
            || false,
        )
        .expect("parse through awkward source boundaries");

        assert_eq!(
            std::fs::read(staging.path().join(result.base_image.staged_id)).expect("read base PNG"),
            expected_base
        );
    }

    #[test]
    fn preserves_non_card_latin1_text_keywords_without_interpreting_them() {
        let latin1_text = chunk(b"tEXt", &[0xa1, 0, b'v', b'a', b'l', b'u', b'e']);
        let input = card_png([latin1_text.clone()]);
        let expected_base = png_with([
            ihdr(),
            chunk(b"IDAT", &[1, 2, 3, 4]),
            latin1_text,
            chunk(b"IEND", &[]),
        ]);
        let staging = tempfile::tempdir().expect("staging directory");

        let result = parse_png_card(
            &mut Cursor::new(input),
            staging.path(),
            PngCardLimits::default(),
            || false,
        )
        .expect("preserve unrecognized Latin-1 metadata");

        assert_eq!(
            std::fs::read(staging.path().join(result.base_image.staged_id)).expect("read base PNG"),
            expected_base
        );
    }

    #[test]
    fn streams_embedded_asset_bytes_and_preserves_the_source_reference() {
        let asset = (0_u32..200_000)
            .map(|value| (value % 251) as u8)
            .collect::<Vec<_>>();
        let asset_chunk = text_chunk("chara-ext-asset_:007", &STANDARD.encode(&asset));
        let input = card_png([asset_chunk]);
        let staging = tempfile::tempdir().expect("staging directory");

        let result = parse_png_card(
            &mut Cursor::new(input),
            staging.path(),
            PngCardLimits::default(),
            || false,
        )
        .expect("parse embedded asset");

        assert_eq!(result.embedded_assets.len(), 1);
        let descriptor = &result.embedded_assets[0];
        assert_eq!(
            descriptor.source_chunk_keyword.as_deref(),
            Some("chara-ext-asset_:007")
        );
        assert_eq!(descriptor.asset_reference.as_deref(), Some("007"));
        assert_eq!(descriptor.source_extension, None);
        assert_eq!(descriptor.source_media_type, None);
        assert_eq!(descriptor.byte_size, asset.len() as u64);
        assert_eq!(descriptor.sha256, hex::encode(Sha256::digest(&asset)));
        assert_eq!(
            std::fs::read(staging.path().join(&descriptor.staged_id)).expect("read staged asset"),
            asset
        );
    }

    #[test]
    fn accepts_identical_duplicate_chunks_but_rejects_conflicts() {
        let card = encoded_card("Risu");
        let identical = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            text_chunk("chara", &card),
            text_chunk("chara", &card),
            text_chunk("chara-ext-asset_:1", &STANDARD.encode(b"same")),
            text_chunk("chara-ext-asset_:1", &STANDARD.encode(b"same")),
            chunk(b"IEND", &[]),
        ]);
        let staging = tempfile::tempdir().expect("staging directory");
        let accepted = parse_png_card(
            &mut Cursor::new(identical),
            staging.path(),
            PngCardLimits::default(),
            || false,
        )
        .expect("accept identical duplicates");
        assert_eq!(accepted.embedded_assets.len(), 1);

        let conflicting_card = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            text_chunk("chara", &encoded_card("One")),
            text_chunk("chara", &encoded_card("Two")),
            chunk(b"IEND", &[]),
        ]);
        assert!(
            assert_failure_cleans_staging(conflicting_card, PngCardLimits::default())
                .to_string()
                .contains("conflicting duplicate chara")
        );

        let conflicting_asset = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            text_chunk("chara-ext-asset_1", &STANDARD.encode(b"one")),
            text_chunk("chara-ext-asset_:1", &STANDARD.encode(b"two")),
            text_chunk("chara", &card),
            chunk(b"IEND", &[]),
        ]);
        assert!(
            assert_failure_cleans_staging(conflicting_asset, PngCardLimits::default())
                .to_string()
                .contains("conflicting duplicate PNG embedded asset 1")
        );
    }

    #[test]
    fn preserves_chara_and_ccv3_for_the_typescript_priority_rule() {
        let chara = encoded_card("V2");
        let ccv3 = STANDARD.encode(r#"{"spec":"chara_card_v3","data":{"name":"V3"}}"#);
        let input = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            text_chunk("chara", &chara),
            text_chunk("ccv3", &ccv3),
            chunk(b"IEND", &[]),
        ]);
        let staging = tempfile::tempdir().expect("staging directory");

        let result = parse_png_card(
            &mut Cursor::new(input),
            staging.path(),
            PngCardLimits::default(),
            || false,
        )
        .expect("preserve both supported card chunks");

        assert_eq!(result.chara.as_deref(), Some(chara.as_str()));
        assert_eq!(result.ccv3.as_deref(), Some(ccv3.as_str()));
    }

    #[test]
    fn validates_only_the_ccv3_selected_over_stale_chara_metadata() {
        let ccv3 = STANDARD.encode(r#"{"spec":"chara_card_v3","data":{"name":"V3"}}"#);
        let stale_chara_values = [
            b"%%%".to_vec(),
            b"rcc||rccv1||%%%||stale-hash||%%%".to_vec(),
            vec![0xff, 0xfe],
        ];

        for stale_chara in stale_chara_values {
            let input = png_with([
                ihdr(),
                chunk(b"IDAT", &[1]),
                text_chunk_bytes("chara", &stale_chara),
                text_chunk("ccv3", &ccv3),
                chunk(b"IEND", &[]),
            ]);
            let staging = tempfile::tempdir().expect("staging directory");

            let result = parse_png_card(
                &mut Cursor::new(input),
                staging.path(),
                PngCardLimits::default(),
                || false,
            )
            .expect("ccv3 must take semantic priority over stale chara metadata");

            assert_eq!(result.ccv3.as_deref(), Some(ccv3.as_str()));
        }
    }

    #[test]
    fn rejects_an_invalid_selected_ccv3_instead_of_falling_back_to_chara() {
        let input = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            text_chunk("chara", &encoded_card("V2")),
            text_chunk("ccv3", "%%%"),
            chunk(b"IEND", &[]),
        ]);

        assert!(
            assert_failure_cleans_staging(input, PngCardLimits::default())
                .to_string()
                .contains("invalid base64")
        );
    }

    #[test]
    fn preserves_the_encrypted_card_envelope_for_the_existing_mapper() {
        let envelope = format!(
            "rcc||rccv1||{}||hash-placeholder||{}",
            STANDARD.encode(b"encrypted bytes"),
            STANDARD.encode(br#"{"usePassword":true}"#)
        );
        let input = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            text_chunk("chara", &envelope),
            chunk(b"IEND", &[]),
        ]);
        let staging = tempfile::tempdir().expect("staging directory");

        let result = parse_png_card(
            &mut Cursor::new(input),
            staging.path(),
            PngCardLimits::default(),
            || false,
        )
        .expect("preserve encrypted card metadata");

        assert_eq!(result.chara.as_deref(), Some(envelope.as_str()));
    }

    #[test]
    fn rejects_invalid_signature_crc_iend_and_source_exhaustion() {
        let mut bad_signature = card_png([]);
        bad_signature[0] = 0;
        assert!(
            assert_failure_cleans_staging(bad_signature, PngCardLimits::default())
                .to_string()
                .contains("signature")
        );

        let mut bad_crc_chunk = text_chunk("chara", &encoded_card("Risu"));
        *bad_crc_chunk.last_mut().expect("CRC byte") ^= 1;
        let bad_crc = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            bad_crc_chunk,
            chunk(b"IEND", &[]),
        ]);
        assert!(
            assert_failure_cleans_staging(bad_crc, PngCardLimits::default())
                .to_string()
                .contains("CRC mismatch")
        );

        let missing_iend = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            text_chunk("chara", &encoded_card("Risu")),
        ]);
        assert!(
            assert_failure_cleans_staging(missing_iend, PngCardLimits::default())
                .to_string()
                .contains("truncated PNG chunk length")
        );

        let mut trailing = card_png([]);
        trailing.push(1);
        assert!(
            assert_failure_cleans_staging(trailing, PngCardLimits::default())
                .to_string()
                .contains("after IEND")
        );

        let mut truncated = card_png([]);
        truncated.truncate(truncated.len() - 2);
        assert!(
            assert_failure_cleans_staging(truncated, PngCardLimits::default())
                .to_string()
                .contains("truncated PNG chunk CRC")
        );
    }

    #[test]
    fn rejects_invalid_base64_before_any_payload_can_escape_staging() {
        let invalid_card = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            text_chunk("chara", "%%%"),
            chunk(b"IEND", &[]),
        ]);
        assert!(
            assert_failure_cleans_staging(invalid_card, PngCardLimits::default())
                .to_string()
                .contains("invalid base64")
        );

        let invalid_asset = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            text_chunk("chara-ext-asset_:1", "%%%"),
            text_chunk("chara", &encoded_card("Risu")),
            chunk(b"IEND", &[]),
        ]);
        assert!(
            assert_failure_cleans_staging(invalid_asset, PngCardLimits::default())
                .to_string()
                .contains("invalid base64")
        );
    }

    #[test]
    fn enforces_metadata_asset_and_descriptor_limits() {
        let limits = PngCardLimits {
            max_card_metadata_bytes: 3,
            ..PngCardLimits::default()
        };
        let oversized_metadata = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            text_chunk("chara", "!!!!"),
            chunk(b"IEND", &[]),
        ]);
        assert!(matches!(
            assert_failure_cleans_staging(oversized_metadata, limits),
            PngCardError::LimitExceeded(_)
        ));

        let first = encoded_card("One");
        let second = STANDARD.encode(r#"{"spec":"chara_card_v3","data":{"name":"Two"}}"#);
        let limits = PngCardLimits {
            max_recognized_metadata_bytes: ("chara\0".len()
                + first.len()
                + "ccv3\0".len()
                + second.len()
                - 1) as u64,
            ..PngCardLimits::default()
        };
        let aggregate_metadata = png_with([
            ihdr(),
            chunk(b"IDAT", &[1]),
            text_chunk("chara", &first),
            text_chunk("ccv3", &second),
            chunk(b"IEND", &[]),
        ]);
        assert!(matches!(
            assert_failure_cleans_staging(aggregate_metadata, limits),
            PngCardError::LimitExceeded(_)
        ));

        let limits = PngCardLimits {
            max_embedded_asset_bytes: 3,
            ..PngCardLimits::default()
        };
        let per_asset = card_png([text_chunk(
            "chara-ext-asset_:1",
            &STANDARD.encode([1, 2, 3, 4]),
        )]);
        assert!(matches!(
            assert_failure_cleans_staging(per_asset, limits),
            PngCardError::LimitExceeded(_)
        ));

        let limits = PngCardLimits {
            max_embedded_asset_bytes: 10,
            max_embedded_asset_total_bytes: 6,
            ..PngCardLimits::default()
        };
        let aggregate_assets = card_png([
            text_chunk("chara-ext-asset_:1", &STANDARD.encode([1, 2, 3, 4])),
            text_chunk("chara-ext-asset_:2", &STANDARD.encode([5, 6, 7, 8])),
        ]);
        assert!(matches!(
            assert_failure_cleans_staging(aggregate_assets, limits),
            PngCardError::LimitExceeded(_)
        ));

        let limits = PngCardLimits {
            max_embedded_asset_bytes: 10,
            max_embedded_asset_total_bytes: 8,
            ..PngCardLimits::default()
        };
        let repeated_asset = text_chunk("chara-ext-asset_:same", &STANDARD.encode([1, 2, 3, 4]));
        let duplicate_assets = card_png([
            repeated_asset.clone(),
            repeated_asset.clone(),
            repeated_asset,
        ]);
        assert!(matches!(
            assert_failure_cleans_staging(duplicate_assets, limits),
            PngCardError::LimitExceeded(_)
        ));

        let limits = PngCardLimits {
            max_embedded_asset_count: 1,
            ..PngCardLimits::default()
        };
        let too_many_assets = card_png([
            text_chunk("chara-ext-asset_:1", &STANDARD.encode([1])),
            text_chunk("chara-ext-asset_:2", &STANDARD.encode([2])),
        ]);
        assert!(matches!(
            assert_failure_cleans_staging(too_many_assets, limits),
            PngCardError::LimitExceeded(_)
        ));
    }

    #[test]
    fn cancellation_removes_base_image_and_completed_assets() {
        let input = card_png([
            text_chunk("chara-ext-asset_:1", &STANDARD.encode(vec![7; 200_000])),
            chunk(b"IDAT", &vec![9; 500_000]),
        ]);
        let staging = tempfile::tempdir().expect("staging directory");
        let checks = Cell::new(0_u32);

        let error = parse_png_card(
            &mut Cursor::new(input),
            staging.path(),
            PngCardLimits::default(),
            || {
                checks.set(checks.get() + 1);
                checks.get() > 20
            },
        )
        .expect_err("cancel parsing");

        assert!(matches!(error, PngCardError::Cancelled));
        assert_eq!(
            std::fs::read_dir(staging.path())
                .expect("read staging directory")
                .count(),
            0
        );
    }

    #[test]
    fn rejects_impossible_chunk_arithmetic_before_reading_a_body() {
        let mut input = PNG_SIGNATURE.to_vec();
        input.extend_from_slice(&0x8000_0000_u32.to_be_bytes());
        input.extend_from_slice(b"IHDR");
        let error = assert_failure_cleans_staging(input, PngCardLimits::default());
        assert!(error.to_string().contains("chunk length exceeds"));
    }
}
