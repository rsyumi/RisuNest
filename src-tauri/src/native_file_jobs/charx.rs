use crc32fast::Hasher as Crc32Hasher;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use zip::ZipArchive;

const COPY_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug)]
pub struct CharXLimits {
    pub max_entries: usize,
    pub max_entry_decoded_bytes: u64,
    pub max_total_decoded_bytes: u64,
    pub max_compression_ratio: u64,
    pub max_metadata_bytes: u64,
}

impl Default for CharXLimits {
    fn default() -> Self {
        Self {
            max_entries: 50_000,
            max_entry_decoded_bytes: 50 * 1024 * 1024,
            max_total_decoded_bytes: 10 * 1024 * 1024 * 1024,
            max_compression_ratio: 1_000,
            max_metadata_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CharXParseErrorCode {
    Io,
    InvalidArchive,
    TooManyEntries,
    EntryTooLarge,
    AggregateTooLarge,
    CompressionRatioExceeded,
    InvalidCrc,
    InvalidPath,
    DuplicatePath,
    SanitizedPathCollision,
    MetadataTooLarge,
    MissingCardMetadata,
    InvalidCardMetadata,
    MissingReferencedAsset,
    Cancelled,
}

#[derive(Debug)]
pub struct CharXParseError {
    code: CharXParseErrorCode,
    message: String,
}

impl CharXParseError {
    fn new(code: CharXParseErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> CharXParseErrorCode {
        self.code
    }
}

impl fmt::Display for CharXParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CharXParseError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CharXContainerKind {
    CharX,
    AppendedCharXJpeg,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrdinaryJpegAssetDescriptor {
    pub original_name: String,
    pub extension: Option<String>,
    pub normalized_extension: Option<String>,
    pub mime_type: String,
    pub byte_length: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CharXAssetReference {
    pub order: usize,
    pub uri: String,
    pub original_name: String,
    pub normalized_name: String,
    pub asset_type: String,
    pub display_name: Option<String>,
    pub declared_extension: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StagedPayloadDescriptor {
    pub original_name: String,
    pub normalized_name: String,
    pub extension: Option<String>,
    pub normalized_extension: Option<String>,
    pub mime_type: String,
    pub decoded_size: u64,
    pub compressed_size: u64,
    pub crc32: u32,
    pub sha256: String,
    pub staged_path: PathBuf,
    pub card_asset_types: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedCharXDescriptor {
    pub container_kind: CharXContainerKind,
    pub archive_offset: u64,
    pub entry_count: usize,
    pub total_decoded_bytes: u64,
    pub card_json: String,
    pub staging_directory: PathBuf,
    pub payloads: Vec<StagedPayloadDescriptor>,
    pub asset_references: Vec<CharXAssetReference>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "classification", rename_all = "camelCase")]
pub enum CharXInspection {
    Card(ParsedCharXDescriptor),
    OrdinaryJpegAsset(OrdinaryJpegAssetDescriptor),
}

#[derive(Clone, Debug)]
struct EntryMetadata {
    index: usize,
    original_name: String,
    normalized_name: String,
    extension: Option<String>,
    normalized_extension: Option<String>,
    compressed_size: u64,
    decoded_size: u64,
    expected_crc32: u32,
    is_directory: bool,
}

struct OwnedStagingDirectory {
    path: PathBuf,
    keep: bool,
}

impl OwnedStagingDirectory {
    fn create(root: &Path) -> Result<Self, CharXParseError> {
        fs::create_dir_all(root).map_err(|error| io_error("create staging root", error))?;
        let path = root.join(format!("charx-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).map_err(|error| io_error("create CharX staging directory", error))?;
        Ok(Self { path, keep: false })
    }

    fn preserve(mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }
}

impl Drop for OwnedStagingDirectory {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

pub fn inspect_charx_file<F>(
    source_path: &Path,
    original_name: &str,
    staging_root: &Path,
    limits: CharXLimits,
    mut is_cancelled: F,
) -> Result<CharXInspection, CharXParseError>
where
    F: FnMut() -> bool,
{
    require_not_cancelled(&mut is_cancelled)?;
    validate_limits(limits)?;

    let source_length = fs::metadata(source_path)
        .map_err(|error| io_error("read CharX source metadata", error))?
        .len();
    let extension = extension_of(original_name);
    let normalized_extension = extension.as_ref().map(|value| value.to_ascii_lowercase());
    let jpeg_name = matches!(normalized_extension.as_deref(), Some("jpg" | "jpeg"));
    let jpeg_signature = has_jpeg_signature(source_path)?;

    if jpeg_name && !jpeg_signature {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidArchive,
            "JPEG input does not have a JPEG signature",
        ));
    }

    let archive_file =
        File::open(source_path).map_err(|error| io_error("open CharX source", error))?;
    let mut archive = match ZipArchive::new(archive_file) {
        Ok(archive) => archive,
        Err(_error) if jpeg_name && jpeg_signature => {
            return Ok(CharXInspection::OrdinaryJpegAsset(
                OrdinaryJpegAssetDescriptor {
                    original_name: original_name.to_owned(),
                    extension,
                    normalized_extension,
                    mime_type: "image/jpeg".to_owned(),
                    byte_length: source_length,
                },
            ));
        }
        Err(error) => return Err(zip_error("open CharX archive", error)),
    };

    let archive_offset = archive.offset();
    let has_card_entry = archive_has_card_entry(&mut archive)?;
    if jpeg_name && (!has_card_entry || archive_offset == 0) {
        return Ok(CharXInspection::OrdinaryJpegAsset(
            OrdinaryJpegAssetDescriptor {
                original_name: original_name.to_owned(),
                extension,
                normalized_extension,
                mime_type: "image/jpeg".to_owned(),
                byte_length: source_length,
            },
        ));
    }

    let container_kind = if archive_offset == 0 {
        CharXContainerKind::CharX
    } else {
        if !jpeg_signature || !jpeg_prefix_has_end_marker(source_path, archive_offset)? {
            return Err(CharXParseError::new(
                CharXParseErrorCode::InvalidArchive,
                "data before the CharX archive is not a complete JPEG prefix",
            ));
        }
        CharXContainerKind::AppendedCharXJpeg
    };

    if !has_card_entry {
        return Err(CharXParseError::new(
            CharXParseErrorCode::MissingCardMetadata,
            "CharX archive has no root card.json entry",
        ));
    }

    let entries = inspect_entries(source_path, &mut archive, limits, &mut is_cancelled)?;
    let staging = OwnedStagingDirectory::create(staging_root)?;
    let mut total_actual_decoded = 0_u64;
    let mut card_json = None;
    let mut payloads = Vec::new();

    for entry in &entries {
        require_not_cancelled(&mut is_cancelled)?;
        if entry.is_directory {
            continue;
        }

        if entry.normalized_name == "card.json" {
            let bytes = read_entry_to_memory(
                &mut archive,
                entry,
                limits.max_metadata_bytes,
                &mut total_actual_decoded,
                limits.max_total_decoded_bytes,
                &mut is_cancelled,
            )?;
            let text = String::from_utf8(bytes).map_err(|_| {
                CharXParseError::new(
                    CharXParseErrorCode::InvalidCardMetadata,
                    "card.json is not valid UTF-8",
                )
            })?;
            card_json = Some(text);
            continue;
        }

        let staged_path = staging
            .path
            .join(format!("{}.payload", uuid::Uuid::new_v4()));
        let payload = stage_entry(
            &mut archive,
            entry,
            &staged_path,
            &mut total_actual_decoded,
            limits.max_total_decoded_bytes,
            &mut is_cancelled,
        )?;
        payloads.push(payload);
    }

    let card_json = card_json.ok_or_else(|| {
        CharXParseError::new(
            CharXParseErrorCode::MissingCardMetadata,
            "CharX archive has no readable card.json entry",
        )
    })?;
    let asset_references = validate_card_metadata(&card_json, &payloads)?;
    let mut types_by_name: HashMap<&str, Vec<String>> = HashMap::new();
    for reference in &asset_references {
        types_by_name
            .entry(&reference.normalized_name)
            .or_default()
            .push(reference.asset_type.clone());
    }
    for payload in &mut payloads {
        payload.card_asset_types = types_by_name
            .remove(payload.normalized_name.as_str())
            .unwrap_or_default();
    }

    let staging_directory = staging.preserve();
    Ok(CharXInspection::Card(ParsedCharXDescriptor {
        container_kind,
        archive_offset,
        entry_count: entries.len(),
        total_decoded_bytes: total_actual_decoded,
        card_json,
        staging_directory,
        payloads,
        asset_references,
    }))
}

fn validate_limits(limits: CharXLimits) -> Result<(), CharXParseError> {
    if limits.max_entries == 0
        || limits.max_entry_decoded_bytes == 0
        || limits.max_total_decoded_bytes == 0
        || limits.max_compression_ratio == 0
        || limits.max_metadata_bytes == 0
    {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidArchive,
            "CharX limits must be nonzero",
        ));
    }
    Ok(())
}

fn archive_has_card_entry<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<bool, CharXParseError> {
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| zip_error("inspect CharX entry", error))?;
        let name = std::str::from_utf8(entry.name_raw()).map_err(|_| {
            CharXParseError::new(
                CharXParseErrorCode::InvalidPath,
                "CharX entry name is not valid UTF-8",
            )
        })?;
        if name == "card.json" {
            return Ok(true);
        }
    }
    Ok(false)
}

fn inspect_entries<R, F>(
    source_path: &Path,
    archive: &mut ZipArchive<R>,
    limits: CharXLimits,
    is_cancelled: &mut F,
) -> Result<Vec<EntryMetadata>, CharXParseError>
where
    R: Read + Seek,
    F: FnMut() -> bool,
{
    if archive.len() > limits.max_entries {
        return Err(CharXParseError::new(
            CharXParseErrorCode::TooManyEntries,
            format!(
                "CharX has {} entries, limit is {}",
                archive.len(),
                limits.max_entries
            ),
        ));
    }

    let mut names = HashSet::with_capacity(archive.len());
    let mut sanitized_names = HashMap::with_capacity(archive.len());
    let mut total_decoded = 0_u64;
    let mut entries = Vec::with_capacity(archive.len());

    for index in 0..archive.len() {
        require_not_cancelled(is_cancelled)?;
        let entry = archive
            .by_index(index)
            .map_err(|error| zip_error("inspect CharX entry", error))?;
        let raw_name = entry.name_raw();
        let local_name = read_local_entry_name(source_path, entry.header_start())?;
        if raw_name != local_name.as_slice() {
            return Err(CharXParseError::new(
                CharXParseErrorCode::InvalidPath,
                "CharX local and central entry names differ",
            ));
        }
        let original_name = std::str::from_utf8(raw_name)
            .map_err(|_| {
                CharXParseError::new(
                    CharXParseErrorCode::InvalidPath,
                    "CharX entry name is not valid UTF-8",
                )
            })?
            .to_owned();
        let is_directory = original_name.ends_with('/');
        let (normalized_name, sanitized_name) =
            validate_archive_name(&original_name, is_directory)?;

        if !names.insert(normalized_name.clone()) {
            return Err(CharXParseError::new(
                CharXParseErrorCode::DuplicatePath,
                format!("duplicate CharX path: {normalized_name}"),
            ));
        }
        if let Some(previous) = sanitized_names.insert(sanitized_name, normalized_name.clone()) {
            return Err(CharXParseError::new(
                CharXParseErrorCode::SanitizedPathCollision,
                format!("CharX paths collide after sanitization: {previous}, {normalized_name}"),
            ));
        }

        let decoded_size = entry.size();
        let compressed_size = entry.compressed_size();
        if decoded_size > limits.max_entry_decoded_bytes {
            return Err(CharXParseError::new(
                CharXParseErrorCode::EntryTooLarge,
                format!("CharX entry exceeds decoded-size limit: {normalized_name}"),
            ));
        }
        total_decoded = total_decoded.checked_add(decoded_size).ok_or_else(|| {
            CharXParseError::new(
                CharXParseErrorCode::AggregateTooLarge,
                "CharX aggregate decoded size overflowed",
            )
        })?;
        if total_decoded > limits.max_total_decoded_bytes {
            return Err(CharXParseError::new(
                CharXParseErrorCode::AggregateTooLarge,
                "CharX aggregate decoded size exceeds its limit",
            ));
        }
        if decoded_size > 0
            && (compressed_size == 0
                || u128::from(decoded_size)
                    > u128::from(compressed_size) * u128::from(limits.max_compression_ratio))
        {
            return Err(CharXParseError::new(
                CharXParseErrorCode::CompressionRatioExceeded,
                format!("CharX entry exceeds compression-ratio limit: {normalized_name}"),
            ));
        }
        if normalized_name == "card.json" && decoded_size > limits.max_metadata_bytes {
            return Err(CharXParseError::new(
                CharXParseErrorCode::MetadataTooLarge,
                "CharX card.json exceeds metadata limit",
            ));
        }

        let extension = extension_of(&original_name);
        let normalized_extension = extension.as_ref().map(|value| value.to_ascii_lowercase());
        entries.push(EntryMetadata {
            index,
            original_name,
            normalized_name,
            extension,
            normalized_extension,
            compressed_size,
            decoded_size,
            expected_crc32: entry.crc32(),
            is_directory,
        });
    }

    Ok(entries)
}

fn validate_archive_name(
    original_name: &str,
    is_directory: bool,
) -> Result<(String, String), CharXParseError> {
    if original_name.is_empty()
        || original_name.contains('\0')
        || original_name.contains('\\')
        || original_name.starts_with('/')
        || has_drive_prefix(original_name)
    {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidPath,
            format!("unsafe CharX entry path: {original_name:?}"),
        ));
    }

    let without_directory_suffix = if is_directory {
        original_name.strip_suffix('/').unwrap_or(original_name)
    } else {
        original_name
    };
    let segments: Vec<&str> = without_directory_suffix.split('/').collect();
    if segments.is_empty()
        || segments
            .iter()
            .any(|segment| segment.is_empty() || *segment == "." || *segment == "..")
    {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidPath,
            format!("unsafe CharX entry path: {original_name:?}"),
        ));
    }

    let mut sanitized_segments = Vec::with_capacity(segments.len());
    for segment in &segments {
        let sanitized: String = segment
            .chars()
            .map(|character| {
                if character <= '\u{1f}'
                    || matches!(character, '<' | '>' | ':' | '"' | '|' | '?' | '*')
                {
                    '_'
                } else {
                    character
                }
            })
            .collect::<String>()
            .trim_end_matches(['.', ' '])
            .to_lowercase();
        if sanitized.is_empty() {
            return Err(CharXParseError::new(
                CharXParseErrorCode::InvalidPath,
                format!("CharX entry path has an empty sanitized segment: {original_name:?}"),
            ));
        }
        sanitized_segments.push(sanitized);
    }

    Ok((segments.join("/"), sanitized_segments.join("/")))
}

fn has_drive_prefix(name: &str) -> bool {
    let bytes = name.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn read_local_entry_name(
    source_path: &Path,
    header_start: u64,
) -> Result<Vec<u8>, CharXParseError> {
    let mut source =
        File::open(source_path).map_err(|error| io_error("open local ZIP header", error))?;
    source
        .seek(SeekFrom::Start(header_start))
        .map_err(|error| io_error("seek local ZIP header", error))?;
    let mut fixed = [0_u8; 30];
    source
        .read_exact(&mut fixed)
        .map_err(|error| io_error("read local ZIP header", error))?;
    if fixed[0..4] != [0x50, 0x4b, 0x03, 0x04] {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidArchive,
            "invalid local ZIP header signature",
        ));
    }
    let name_length = u16::from_le_bytes([fixed[26], fixed[27]]) as usize;
    let mut name = vec![0_u8; name_length];
    source
        .read_exact(&mut name)
        .map_err(|error| io_error("read local ZIP entry name", error))?;
    Ok(name)
}

fn read_entry_to_memory<R, F>(
    archive: &mut ZipArchive<R>,
    entry: &EntryMetadata,
    byte_limit: u64,
    aggregate_actual: &mut u64,
    aggregate_limit: u64,
    is_cancelled: &mut F,
) -> Result<Vec<u8>, CharXParseError>
where
    R: Read + Seek,
    F: FnMut() -> bool,
{
    let capacity = usize::try_from(entry.decoded_size).map_err(|_| {
        CharXParseError::new(
            CharXParseErrorCode::MetadataTooLarge,
            "card.json cannot fit in memory on this platform",
        )
    })?;
    let mut output = Vec::with_capacity(capacity);
    read_entry(
        archive,
        entry,
        byte_limit,
        aggregate_actual,
        aggregate_limit,
        is_cancelled,
        |bytes| {
            output.extend_from_slice(bytes);
            Ok(())
        },
    )?;
    Ok(output)
}

fn stage_entry<R, F>(
    archive: &mut ZipArchive<R>,
    entry: &EntryMetadata,
    staged_path: &Path,
    aggregate_actual: &mut u64,
    aggregate_limit: u64,
    is_cancelled: &mut F,
) -> Result<StagedPayloadDescriptor, CharXParseError>
where
    R: Read + Seek,
    F: FnMut() -> bool,
{
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(staged_path)
        .map_err(|error| io_error("create staged CharX payload", error))?;
    let mut writer = BufWriter::with_capacity(COPY_BUFFER_BYTES, file);
    let mut sniff = Vec::with_capacity(512);
    let mut sha256 = Sha256::new();
    let (decoded_size, crc32) = read_entry(
        archive,
        entry,
        entry.decoded_size,
        aggregate_actual,
        aggregate_limit,
        is_cancelled,
        |bytes| {
            if sniff.len() < 512 {
                let count = (512 - sniff.len()).min(bytes.len());
                sniff.extend_from_slice(&bytes[..count]);
            }
            sha256.update(bytes);
            writer
                .write_all(bytes)
                .map_err(|error| io_error("write staged CharX payload", error))
        },
    )?;
    writer
        .flush()
        .map_err(|error| io_error("flush staged CharX payload", error))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|error| io_error("sync staged CharX payload", error))?;

    Ok(StagedPayloadDescriptor {
        original_name: entry.original_name.clone(),
        normalized_name: entry.normalized_name.clone(),
        extension: entry.extension.clone(),
        normalized_extension: entry.normalized_extension.clone(),
        mime_type: detect_mime(&sniff, entry.normalized_extension.as_deref()).to_owned(),
        decoded_size,
        compressed_size: entry.compressed_size,
        crc32,
        sha256: hex::encode(sha256.finalize()),
        staged_path: staged_path.to_owned(),
        card_asset_types: Vec::new(),
    })
}

fn read_entry<R, F, W>(
    archive: &mut ZipArchive<R>,
    metadata: &EntryMetadata,
    byte_limit: u64,
    aggregate_actual: &mut u64,
    aggregate_limit: u64,
    is_cancelled: &mut F,
    mut write_chunk: W,
) -> Result<(u64, u32), CharXParseError>
where
    R: Read + Seek,
    F: FnMut() -> bool,
    W: FnMut(&[u8]) -> Result<(), CharXParseError>,
{
    let mut entry = archive
        .by_index(metadata.index)
        .map_err(|error| zip_error("open CharX entry", error))?;
    let mut crc32 = Crc32Hasher::new();
    let mut actual = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];

    loop {
        require_not_cancelled(is_cancelled)?;
        let count = entry.read(&mut buffer).map_err(|error| {
            if error.to_string().contains("Invalid checksum") {
                CharXParseError::new(
                    CharXParseErrorCode::InvalidCrc,
                    format!("CharX CRC mismatch: {}", metadata.normalized_name),
                )
            } else {
                io_error("decode CharX entry", error)
            }
        })?;
        if count == 0 {
            break;
        }
        let count_u64 = count as u64;
        actual = actual.checked_add(count_u64).ok_or_else(|| {
            CharXParseError::new(
                CharXParseErrorCode::EntryTooLarge,
                "CharX decoded entry size overflowed",
            )
        })?;
        if actual > byte_limit || actual > metadata.decoded_size {
            return Err(CharXParseError::new(
                if metadata.normalized_name == "card.json" {
                    CharXParseErrorCode::MetadataTooLarge
                } else {
                    CharXParseErrorCode::EntryTooLarge
                },
                format!(
                    "CharX entry decoded past its size limit: {}",
                    metadata.normalized_name
                ),
            ));
        }
        let next_aggregate = aggregate_actual.checked_add(count_u64).ok_or_else(|| {
            CharXParseError::new(
                CharXParseErrorCode::AggregateTooLarge,
                "CharX decoded aggregate size overflowed",
            )
        })?;
        if next_aggregate > aggregate_limit {
            return Err(CharXParseError::new(
                CharXParseErrorCode::AggregateTooLarge,
                "CharX decoded aggregate exceeds its limit",
            ));
        }
        write_chunk(&buffer[..count])?;
        crc32.update(&buffer[..count]);
        *aggregate_actual = next_aggregate;
    }

    if actual != metadata.decoded_size {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidArchive,
            format!(
                "CharX entry decoded size differs from its directory: {}",
                metadata.normalized_name
            ),
        ));
    }
    let actual_crc32 = crc32.finalize();
    if actual_crc32 != metadata.expected_crc32 {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidCrc,
            format!("CharX CRC mismatch: {}", metadata.normalized_name),
        ));
    }
    Ok((actual, actual_crc32))
}

fn validate_card_metadata(
    card_json: &str,
    payloads: &[StagedPayloadDescriptor],
) -> Result<Vec<CharXAssetReference>, CharXParseError> {
    let card: Value = serde_json::from_str(card_json).map_err(|error| {
        CharXParseError::new(
            CharXParseErrorCode::InvalidCardMetadata,
            format!("card.json is invalid JSON: {error}"),
        )
    })?;
    if card.get("spec").and_then(Value::as_str) != Some("chara_card_v3")
        || !card.get("data").is_some_and(Value::is_object)
    {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidCardMetadata,
            "card.json is not a Character Card V3 object",
        ));
    }

    let payload_names: HashSet<&str> = payloads
        .iter()
        .map(|payload| payload.normalized_name.as_str())
        .collect();
    let mut references = Vec::new();
    let Some(assets) = card
        .get("data")
        .and_then(|data| data.get("assets"))
        .and_then(Value::as_array)
    else {
        return Ok(references);
    };

    for (order, asset) in assets.iter().enumerate() {
        let Some(uri) = asset.get("uri").and_then(Value::as_str) else {
            continue;
        };
        let Some(original_name) = uri.strip_prefix("embeded://") else {
            continue;
        };
        let (normalized_name, _) = validate_archive_name(original_name, false).map_err(|_| {
            CharXParseError::new(
                CharXParseErrorCode::InvalidCardMetadata,
                format!("card.json contains an unsafe embedded asset URI: {uri}"),
            )
        })?;
        if !payload_names.contains(normalized_name.as_str()) {
            return Err(CharXParseError::new(
                CharXParseErrorCode::MissingReferencedAsset,
                format!("card.json references a missing CharX entry: {original_name}"),
            ));
        }
        references.push(CharXAssetReference {
            order,
            uri: uri.to_owned(),
            original_name: original_name.to_owned(),
            normalized_name,
            asset_type: asset
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("asset")
                .to_owned(),
            display_name: asset.get("name").and_then(Value::as_str).map(str::to_owned),
            declared_extension: asset.get("ext").and_then(Value::as_str).map(str::to_owned),
        });
    }
    Ok(references)
}

fn extension_of(name: &str) -> Option<String> {
    let final_segment = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let separator = final_segment.rfind('.')?;
    if separator == 0 || separator + 1 == final_segment.len() {
        return None;
    }
    Some(final_segment[separator + 1..].to_owned())
}

fn has_jpeg_signature(path: &Path) -> Result<bool, CharXParseError> {
    let mut file = File::open(path).map_err(|error| io_error("open JPEG source", error))?;
    let mut signature = [0_u8; 3];
    match file.read_exact(&mut signature) {
        Ok(()) => Ok(signature == [0xff, 0xd8, 0xff]),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(io_error("read JPEG signature", error)),
    }
}

fn jpeg_prefix_has_end_marker(path: &Path, archive_offset: u64) -> Result<bool, CharXParseError> {
    let mut file = File::open(path).map_err(|error| io_error("open appended JPEG", error))?;
    let mut remaining = archive_offset;
    let mut previous = None;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        let requested = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64)).unwrap();
        let count = file
            .read(&mut buffer[..requested])
            .map_err(|error| io_error("read appended JPEG prefix", error))?;
        if count == 0 {
            return Ok(false);
        }
        for &byte in &buffer[..count] {
            if previous == Some(0xff) && byte == 0xd9 {
                return Ok(true);
            }
            previous = Some(byte);
        }
        remaining -= count as u64;
    }
    Ok(false)
}

fn detect_mime(prefix: &[u8], extension: Option<&str>) -> &'static str {
    if prefix.starts_with(b"\x89PNG\r\n\x1a\n") {
        return "image/png";
    }
    if prefix.starts_with(&[0xff, 0xd8, 0xff]) {
        return "image/jpeg";
    }
    if prefix.starts_with(b"GIF87a") || prefix.starts_with(b"GIF89a") {
        return "image/gif";
    }
    if prefix.len() >= 12 && &prefix[0..4] == b"RIFF" && &prefix[8..12] == b"WEBP" {
        return "image/webp";
    }
    if prefix.len() >= 12 && &prefix[4..8] == b"ftyp" {
        if matches!(&prefix[8..12], b"avif" | b"avis") {
            return "image/avif";
        }
        return "video/mp4";
    }
    if prefix.starts_with(b"OggS") {
        return "audio/ogg";
    }
    if prefix.len() >= 12 && &prefix[0..4] == b"RIFF" && &prefix[8..12] == b"WAVE" {
        return "audio/wav";
    }
    if prefix.starts_with(b"ID3") {
        return "audio/mpeg";
    }
    if prefix.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return "video/webm";
    }

    match extension {
        Some("json") => "application/json",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif" | "avis") => "image/avif",
        Some("svg") => "image/svg+xml",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("ogg") => "audio/ogg",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("risum") => "application/x-risum",
        _ => "application/octet-stream",
    }
}

fn require_not_cancelled<F>(is_cancelled: &mut F) -> Result<(), CharXParseError>
where
    F: FnMut() -> bool,
{
    if is_cancelled() {
        return Err(CharXParseError::new(
            CharXParseErrorCode::Cancelled,
            "CharX parsing was cancelled",
        ));
    }
    Ok(())
}

fn io_error(context: &str, error: io::Error) -> CharXParseError {
    CharXParseError::new(CharXParseErrorCode::Io, format!("{context}: {error}"))
}

fn zip_error(context: &str, error: zip::result::ZipError) -> CharXParseError {
    CharXParseError::new(
        CharXParseErrorCode::InvalidArchive,
        format!("{context}: {error}"),
    )
}
