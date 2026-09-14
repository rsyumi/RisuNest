use crate::import_export_jobs::classifier::{preflight_zip, ArchiveView, ZipPreflight};
use crate::import_export_jobs::{FormatError, FormatErrorKind};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use crc32fast::Hasher as Crc32Hasher;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use url::Url;
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter, SUPPORTED_COMPRESSION_METHODS};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const CANCELLED_IO_MESSAGE: &str = "CharX parsing was cancelled";
const CENTRAL_DIRECTORY_SIGNATURE: u32 = 0x0201_4b50;
const CENTRAL_DIRECTORY_HEADER_BYTES: usize = 46;
const LOCAL_FILE_HEADER_SIGNATURE: u32 = 0x0403_4b50;
const LOCAL_FILE_HEADER_BYTES: usize = 30;
const DATA_DESCRIPTOR_SIGNATURE: u32 = 0x0807_4b50;
const ZIP64_EXTRA_FIELD_KIND: u16 = 0x0001;
const AES_EXTRA_FIELD_KIND: u16 = 0x9901;
const AES_COMPRESSION_METHOD: u16 = 99;
const DEFLATE_COMPRESSION_METHOD: u16 = 8;

#[derive(Clone, Copy, Debug)]
pub struct CharXLimits {
    pub max_entries: usize,
    pub max_directory_bytes: u64,
    pub max_entry_decoded_bytes: u64,
    pub max_total_decoded_bytes: u64,
    pub max_compression_ratio: u64,
    pub max_metadata_bytes: u64,
}

impl Default for CharXLimits {
    fn default() -> Self {
        Self {
            max_entries: 50_000,
            max_directory_bytes: 256 * 1024 * 1024,
            max_entry_decoded_bytes: 50 * 1024 * 1024,
            max_total_decoded_bytes: 10 * 1024 * 1024 * 1024,
            max_compression_ratio: 1_000,
            max_metadata_bytes: crate::import_export_jobs::MAX_CONTENT_METADATA_BYTES as u64,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CharXWriteErrorCode {
    Io,
    InvalidDescriptor,
    PayloadHashMismatch,
    Cancelled,
}

#[derive(Debug)]
pub struct CharXWriteError {
    code: CharXWriteErrorCode,
    message: String,
}

impl CharXWriteError {
    fn new(code: CharXWriteErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> CharXWriteErrorCode {
        self.code
    }
}

impl fmt::Display for CharXWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CharXWriteError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WrittenCharXFile {
    pub path: PathBuf,
    pub byte_length: u64,
    pub sha256: String,
}

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

struct OwnedCharXOutput {
    temporary: PathBuf,
    keep: bool,
}

impl OwnedCharXOutput {
    fn new(temporary: PathBuf) -> Self {
        Self {
            temporary,
            keep: false,
        }
    }

    fn preserve(mut self) {
        self.keep = true;
    }
}

impl Drop for OwnedCharXOutput {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_file(&self.temporary);
        }
    }
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

#[derive(Clone, Copy, Debug)]
struct EntryLayout {
    local_start: u64,
    local_end: u64,
    central_start: u64,
    central_end: u64,
}

#[derive(Clone, Copy, Debug)]
struct DataDescriptorExpected {
    crc32: u32,
    compressed: u64,
    decoded: u64,
    uses_zip64: bool,
}

struct OwnedStagingDirectory {
    path: PathBuf,
    keep: bool,
}

struct Cancellation<F> {
    callback: Rc<RefCell<F>>,
}

impl<F> Clone for Cancellation<F> {
    fn clone(&self) -> Self {
        Self {
            callback: Rc::clone(&self.callback),
        }
    }
}

impl<F> Cancellation<F>
where
    F: FnMut() -> bool,
{
    fn new(callback: F) -> Self {
        Self {
            callback: Rc::new(RefCell::new(callback)),
        }
    }

    fn check(&self) -> Result<(), CharXParseError> {
        if self.is_cancelled() {
            return Err(CharXParseError::new(
                CharXParseErrorCode::Cancelled,
                CANCELLED_IO_MESSAGE,
            ));
        }
        Ok(())
    }

    fn is_cancelled(&self) -> bool {
        (self.callback.borrow_mut())()
    }

    fn check_io(&self) -> io::Result<()> {
        self.check()
            .map_err(|_| io::Error::other(CANCELLED_IO_MESSAGE))
    }
}

struct CancellableReader<R, F> {
    inner: R,
    cancellation: Cancellation<F>,
}

impl<R, F> CancellableReader<R, F> {
    fn new(inner: R, cancellation: Cancellation<F>) -> Self {
        Self {
            inner,
            cancellation,
        }
    }
}

impl<R, F> Read for CancellableReader<R, F>
where
    R: Read,
    F: FnMut() -> bool,
{
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.cancellation.check_io()?;
        self.inner.read(buffer)
    }
}

impl<R, F> Seek for CancellableReader<R, F>
where
    R: Seek,
    F: FnMut() -> bool,
{
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.cancellation.check_io()?;
        self.inner.seek(position)
    }
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
    is_cancelled: F,
) -> Result<CharXInspection, CharXParseError>
where
    F: FnMut() -> bool,
{
    let source = File::open(source_path).map_err(|error| io_error("open CharX source", error))?;
    inspect_charx_opened_file(&source, original_name, staging_root, limits, is_cancelled)
}

pub fn inspect_charx_opened_file<F>(
    source_file: &File,
    original_name: &str,
    staging_root: &Path,
    limits: CharXLimits,
    is_cancelled: F,
) -> Result<CharXInspection, CharXParseError>
where
    F: FnMut() -> bool,
{
    let cancellation = Cancellation::new(is_cancelled);
    cancellation.check()?;
    validate_limits(limits)?;

    let source_length = source_file
        .metadata()
        .map_err(|error| io_error("read CharX source metadata", error))?
        .len();
    let extension = extension_of(original_name);
    let normalized_extension = extension.as_ref().map(|value| value.to_ascii_lowercase());
    let jpeg_signature = has_jpeg_signature(source_file, &cancellation)?;

    match inspect_card_container(
        source_file,
        staging_root,
        limits,
        jpeg_signature,
        &cancellation,
    ) {
        Ok(card) => Ok(CharXInspection::Card(card)),
        Err(error)
            if jpeg_signature
                && !matches!(
                    error.code(),
                    CharXParseErrorCode::Cancelled | CharXParseErrorCode::Io
                ) =>
        {
            cancellation.check()?;
            Ok(ordinary_jpeg_descriptor(
                original_name,
                &extension,
                &normalized_extension,
                source_length,
            ))
        }
        Err(error) => Err(error),
    }
}

pub fn write_charx_file<F>(
    descriptor: &ParsedCharXDescriptor,
    output_root: &Path,
    is_cancelled: F,
) -> Result<WrittenCharXFile, CharXWriteError>
where
    F: FnMut() -> bool,
{
    let cancellation = Cancellation::new(is_cancelled);
    check_write_cancellation(&cancellation)?;
    validate_export_descriptor(descriptor)?;
    let output_root = validate_plain_directory(output_root, "CharX export root")?;
    let staging_root =
        validate_plain_directory(&descriptor.staging_directory, "CharX staging root")?;
    let output_id = uuid::Uuid::new_v4();
    let temporary = output_root.join(format!("charx-{output_id}.tmp"));
    let final_path = output_root.join(format!("charx-{output_id}.charx"));
    let file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| write_io_error("create CharX export", error))?;
    let output_guard = OwnedCharXOutput::new(temporary.clone());
    let mut archive = ZipWriter::new(BufWriter::with_capacity(COPY_BUFFER_BYTES, file));

    for payload in &descriptor.payloads {
        check_write_cancellation(&cancellation)?;
        let source = open_staged_payload(&staging_root, payload)?;
        let options = FileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(payload.decoded_size >= u64::from(u32::MAX));
        archive
            .start_file(&payload.original_name, options)
            .map_err(|error| write_zip_error("start CharX payload", error))?;
        copy_verified_payload(&mut archive, source, payload, &cancellation)?;
    }

    check_write_cancellation(&cancellation)?;
    archive
        .start_file(
            "card.json",
            FileOptions::default().compression_method(CompressionMethod::Stored),
        )
        .map_err(|error| write_zip_error("start CharX card metadata", error))?;
    write_cancellable_bytes(
        &mut archive,
        descriptor.card_json.as_bytes(),
        &cancellation,
        "write CharX card metadata",
    )?;
    check_write_cancellation(&cancellation)?;
    let mut writer = archive
        .finish()
        .map_err(|error| write_zip_error("finish CharX archive", error))?;
    writer
        .flush()
        .map_err(|error| write_io_error("flush CharX archive", error))?;
    writer
        .get_ref()
        .sync_all()
        .map_err(|error| write_io_error("sync CharX archive", error))?;
    drop(writer);

    let (byte_length, sha256) = hash_file(&temporary, &cancellation)?;
    check_write_cancellation(&cancellation)?;
    fs::rename(&temporary, &final_path)
        .map_err(|error| write_io_error("publish CharX export", error))?;
    output_guard.preserve();
    Ok(WrittenCharXFile {
        path: final_path,
        byte_length,
        sha256,
    })
}

fn validate_export_descriptor(descriptor: &ParsedCharXDescriptor) -> Result<(), CharXWriteError> {
    let mut names = HashSet::with_capacity(descriptor.payloads.len() + 1);
    let mut sanitized_names = HashMap::with_capacity(descriptor.payloads.len() + 1);
    names.insert("card.json".to_owned());
    sanitized_names.insert("card.json".to_owned(), "card.json".to_owned());
    let mut entries = Vec::with_capacity(descriptor.payloads.len() + 1);
    for (index, payload) in descriptor.payloads.iter().enumerate() {
        let (normalized_name, sanitized_name) =
            validate_archive_name(&payload.original_name, false).map_err(|error| {
                CharXWriteError::new(CharXWriteErrorCode::InvalidDescriptor, error.to_string())
            })?;
        if normalized_name == "card.json" || normalized_name != payload.normalized_name {
            return Err(CharXWriteError::new(
                CharXWriteErrorCode::InvalidDescriptor,
                "CharX payload descriptor has an inconsistent archive name",
            ));
        }
        if !names.insert(normalized_name.clone()) {
            return Err(CharXWriteError::new(
                CharXWriteErrorCode::InvalidDescriptor,
                "CharX payload descriptor contains a duplicate archive name",
            ));
        }
        if sanitized_names
            .insert(sanitized_name, normalized_name.clone())
            .is_some()
        {
            return Err(CharXWriteError::new(
                CharXWriteErrorCode::InvalidDescriptor,
                "CharX payload descriptors collide after path sanitization",
            ));
        }
        let extension = extension_of(&payload.original_name);
        let normalized_extension = extension.as_ref().map(|value| value.to_ascii_lowercase());
        if extension != payload.extension || normalized_extension != payload.normalized_extension {
            return Err(CharXWriteError::new(
                CharXWriteErrorCode::InvalidDescriptor,
                "CharX payload descriptor has inconsistent extension metadata",
            ));
        }
        entries.push(EntryMetadata {
            index,
            original_name: payload.original_name.clone(),
            normalized_name,
            extension,
            normalized_extension,
            compressed_size: payload.compressed_size,
            decoded_size: payload.decoded_size,
            expected_crc32: payload.crc32,
            is_directory: false,
        });
    }
    entries.push(EntryMetadata {
        index: descriptor.payloads.len(),
        original_name: "card.json".to_owned(),
        normalized_name: "card.json".to_owned(),
        extension: Some("json".to_owned()),
        normalized_extension: Some("json".to_owned()),
        compressed_size: 0,
        decoded_size: descriptor.card_json.len() as u64,
        expected_crc32: 0,
        is_directory: false,
    });
    let references = validate_card_metadata(&descriptor.card_json, &entries).map_err(|error| {
        CharXWriteError::new(CharXWriteErrorCode::InvalidDescriptor, error.to_string())
    })?;
    if references != descriptor.asset_references {
        return Err(CharXWriteError::new(
            CharXWriteErrorCode::InvalidDescriptor,
            "CharX card references differ from its parsed descriptor",
        ));
    }
    let mut types_by_name: HashMap<&str, Vec<String>> = HashMap::new();
    for reference in &references {
        types_by_name
            .entry(&reference.normalized_name)
            .or_default()
            .push(reference.asset_type.clone());
    }
    if descriptor.payloads.iter().any(|payload| {
        types_by_name
            .remove(payload.normalized_name.as_str())
            .unwrap_or_default()
            != payload.card_asset_types
    }) {
        return Err(CharXWriteError::new(
            CharXWriteErrorCode::InvalidDescriptor,
            "CharX payload ownership differs from its card references",
        ));
    }
    Ok(())
}

fn validate_plain_directory(path: &Path, label: &str) -> Result<PathBuf, CharXWriteError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| write_io_error(&format!("read {label}"), error))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(CharXWriteError::new(
            CharXWriteErrorCode::InvalidDescriptor,
            format!("{label} must be a plain directory"),
        ));
    }
    path.canonicalize()
        .map_err(|error| write_io_error(&format!("resolve {label}"), error))
}

fn open_staged_payload(
    staging_root: &Path,
    payload: &StagedPayloadDescriptor,
) -> Result<File, CharXWriteError> {
    let metadata = fs::symlink_metadata(&payload.staged_path)
        .map_err(|error| write_io_error("read staged CharX payload", error))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(CharXWriteError::new(
            CharXWriteErrorCode::InvalidDescriptor,
            "staged CharX payload must be a plain file",
        ));
    }
    let canonical = payload
        .staged_path
        .canonicalize()
        .map_err(|error| write_io_error("resolve staged CharX payload", error))?;
    if canonical.parent() != Some(staging_root) {
        return Err(CharXWriteError::new(
            CharXWriteErrorCode::InvalidDescriptor,
            "staged CharX payload escapes its owned directory",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options
        .open(canonical)
        .map_err(|error| write_io_error("open staged CharX payload", error))
}

fn write_cancellable_bytes<W, F>(
    destination: &mut W,
    bytes: &[u8],
    cancellation: &Cancellation<F>,
    operation: &str,
) -> Result<(), CharXWriteError>
where
    W: Write,
    F: FnMut() -> bool,
{
    for chunk in bytes.chunks(COPY_BUFFER_BYTES) {
        check_write_cancellation(cancellation)?;
        destination
            .write_all(chunk)
            .map_err(|error| write_io_error(operation, error))?;
    }
    Ok(())
}

fn copy_verified_payload<W, F>(
    destination: &mut W,
    mut source: File,
    payload: &StagedPayloadDescriptor,
    cancellation: &Cancellation<F>,
) -> Result<(), CharXWriteError>
where
    W: Write,
    F: FnMut() -> bool,
{
    let mut sha256 = Sha256::new();
    let mut crc32 = Crc32Hasher::new();
    let mut sniff = Vec::with_capacity(512);
    let mut actual = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        check_write_cancellation(cancellation)?;
        let count = source
            .read(&mut buffer)
            .map_err(|error| write_io_error("read staged CharX payload", error))?;
        if count == 0 {
            break;
        }
        actual = actual.checked_add(count as u64).ok_or_else(|| {
            CharXWriteError::new(
                CharXWriteErrorCode::PayloadHashMismatch,
                "staged CharX payload size overflowed",
            )
        })?;
        if sniff.len() < 512 {
            let sniff_count = (512 - sniff.len()).min(count);
            sniff.extend_from_slice(&buffer[..sniff_count]);
        }
        sha256.update(&buffer[..count]);
        crc32.update(&buffer[..count]);
        destination
            .write_all(&buffer[..count])
            .map_err(|error| write_io_error("write CharX payload", error))?;
    }
    let actual_sha256 = hex::encode(sha256.finalize());
    let actual_crc32 = crc32.finalize();
    let actual_mime = detect_mime(&sniff, payload.normalized_extension.as_deref());
    if actual != payload.decoded_size
        || actual_sha256 != payload.sha256
        || actual_crc32 != payload.crc32
        || actual_mime != payload.mime_type
    {
        return Err(CharXWriteError::new(
            CharXWriteErrorCode::PayloadHashMismatch,
            format!(
                "staged CharX payload no longer matches its descriptor: {}",
                payload.original_name
            ),
        ));
    }
    Ok(())
}

fn hash_file<F>(
    path: &Path,
    cancellation: &Cancellation<F>,
) -> Result<(u64, String), CharXWriteError>
where
    F: FnMut() -> bool,
{
    let mut file = File::open(path).map_err(|error| write_io_error("open CharX export", error))?;
    let mut sha256 = Sha256::new();
    let mut byte_length = 0_u64;
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        check_write_cancellation(cancellation)?;
        let count = file
            .read(&mut buffer)
            .map_err(|error| write_io_error("hash CharX export", error))?;
        if count == 0 {
            break;
        }
        byte_length = byte_length.checked_add(count as u64).ok_or_else(|| {
            CharXWriteError::new(CharXWriteErrorCode::Io, "CharX export size overflowed")
        })?;
        sha256.update(&buffer[..count]);
    }
    Ok((byte_length, hex::encode(sha256.finalize())))
}

fn check_write_cancellation<F>(cancellation: &Cancellation<F>) -> Result<(), CharXWriteError>
where
    F: FnMut() -> bool,
{
    if cancellation.is_cancelled() {
        return Err(CharXWriteError::new(
            CharXWriteErrorCode::Cancelled,
            "CharX export was cancelled",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

fn write_io_error(operation: &str, error: io::Error) -> CharXWriteError {
    CharXWriteError::new(CharXWriteErrorCode::Io, format!("{operation}: {error}"))
}

fn write_zip_error(operation: &str, error: zip::result::ZipError) -> CharXWriteError {
    CharXWriteError::new(CharXWriteErrorCode::Io, format!("{operation}: {error}"))
}

fn inspect_card_container<F>(
    source_file: &File,
    staging_root: &Path,
    limits: CharXLimits,
    jpeg_signature: bool,
    cancellation: &Cancellation<F>,
) -> Result<ParsedCharXDescriptor, CharXParseError>
where
    F: FnMut() -> bool,
{
    let file = source_file
        .try_clone()
        .map_err(|error| io_error("open CharX source", error))?;
    let mut source = CancellableReader::new(file, cancellation.clone());
    let preflight = preflight_zip(&mut source, usize::MAX, u64::MAX, &|| {
        cancellation.is_cancelled()
    })
    .map_err(format_error)?
    .ok_or_else(|| {
        CharXParseError::new(
            CharXParseErrorCode::InvalidArchive,
            "source has no valid bounded ZIP footer",
        )
    })?;

    validate_preflight_limits(preflight, limits)?;
    let container_kind = if jpeg_signature {
        if preflight.archive_start == 0
            || !jpeg_prefix_ends_at_archive(source_file, preflight.archive_start, cancellation)?
        {
            return Err(CharXParseError::new(
                CharXParseErrorCode::InvalidArchive,
                "JPEG prefix does not end immediately before the CharX archive",
            ));
        }
        CharXContainerKind::AppendedCharXJpeg
    } else {
        if preflight.archive_start != 0 {
            return Err(CharXParseError::new(
                CharXParseErrorCode::InvalidArchive,
                "non-JPEG data precedes the CharX archive",
            ));
        }
        CharXContainerKind::CharX
    };

    let view = ArchiveView::new(&mut source, preflight.archive_start, preflight.archive_end)
        .map_err(format_error)?;
    let mut archive =
        ZipArchive::new(view).map_err(|error| structural_zip_error("open CharX archive", error))?;
    cancellation.check()?;
    if archive.offset() != 0 || archive.len() != preflight.entry_count {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidArchive,
            "CharX footer and bounded archive view disagree",
        ));
    }

    match archive
        .file_names()
        .filter(|name| *name == "card.json")
        .count()
    {
        0 => {
            return Err(CharXParseError::new(
                CharXParseErrorCode::MissingCardMetadata,
                "CharX archive has no root card.json entry",
            ))
        }
        1 => {}
        _ => {
            return Err(CharXParseError::new(
                CharXParseErrorCode::DuplicatePath,
                "CharX archive has duplicate root card.json entries",
            ))
        }
    }

    let entries = inspect_entries(source_file, preflight, &mut archive, limits, cancellation)?;
    let card_entry = entries
        .iter()
        .find(|entry| entry.normalized_name == "card.json")
        .ok_or_else(|| {
            CharXParseError::new(
                CharXParseErrorCode::MissingCardMetadata,
                "CharX archive has no readable root card.json entry",
            )
        })?;
    let mut total_actual_decoded = 0_u64;
    let card_bytes = read_entry_to_memory(
        &mut archive,
        card_entry,
        limits.max_metadata_bytes,
        &mut total_actual_decoded,
        limits.max_total_decoded_bytes,
        cancellation,
    )?;
    let card_json = String::from_utf8(card_bytes).map_err(|_| {
        CharXParseError::new(
            CharXParseErrorCode::InvalidCardMetadata,
            "card.json is not valid UTF-8",
        )
    })?;
    let asset_references = validate_card_metadata(&card_json, &entries)?;

    let staging = OwnedStagingDirectory::create(staging_root)?;
    let mut payloads = Vec::new();
    for entry in &entries {
        cancellation.check()?;
        if entry.is_directory || entry.normalized_name == "card.json" {
            continue;
        }
        let staged_path = staging
            .path
            .join(format!("{}.payload", uuid::Uuid::new_v4()));
        payloads.push(stage_entry(
            &mut archive,
            entry,
            &staged_path,
            &mut total_actual_decoded,
            limits.max_total_decoded_bytes,
            cancellation,
        )?);
    }

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

    cancellation.check()?;
    let staging_directory = staging.preserve();
    Ok(ParsedCharXDescriptor {
        container_kind,
        archive_offset: preflight.archive_start,
        entry_count: entries.len(),
        total_decoded_bytes: total_actual_decoded,
        card_json,
        staging_directory,
        payloads,
        asset_references,
    })
}

fn validate_limits(limits: CharXLimits) -> Result<(), CharXParseError> {
    if limits.max_entries == 0
        || limits.max_directory_bytes == 0
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

fn validate_preflight_limits(
    preflight: ZipPreflight,
    limits: CharXLimits,
) -> Result<(), CharXParseError> {
    if preflight.entry_count > limits.max_entries {
        return Err(CharXParseError::new(
            CharXParseErrorCode::TooManyEntries,
            format!(
                "CharX has {} entries, limit is {}",
                preflight.entry_count, limits.max_entries
            ),
        ));
    }
    if preflight.directory_size > limits.max_directory_bytes {
        return Err(CharXParseError::new(
            CharXParseErrorCode::MetadataTooLarge,
            "CharX central directory exceeds its byte limit",
        ));
    }
    Ok(())
}

fn format_error(error: FormatError) -> CharXParseError {
    let code = match error.kind {
        FormatErrorKind::Cancelled => CharXParseErrorCode::Cancelled,
        FormatErrorKind::Io => CharXParseErrorCode::Io,
        FormatErrorKind::InvalidFormat | FormatErrorKind::LimitExceeded => {
            CharXParseErrorCode::InvalidArchive
        }
    };
    CharXParseError::new(code, error.to_string())
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let value = bytes.get(offset..offset.checked_add(2)?)?;
    Some(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let value = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(value.try_into().ok()?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let value = bytes.get(offset..offset.checked_add(8)?)?;
    Some(u64::from_le_bytes(value.try_into().ok()?))
}

fn ordinary_jpeg_descriptor(
    original_name: &str,
    extension: &Option<String>,
    normalized_extension: &Option<String>,
    byte_length: u64,
) -> CharXInspection {
    CharXInspection::OrdinaryJpegAsset(OrdinaryJpegAssetDescriptor {
        original_name: original_name.to_owned(),
        extension: extension.clone(),
        normalized_extension: normalized_extension.clone(),
        mime_type: "image/jpeg".to_owned(),
        byte_length,
    })
}

fn inspect_entries<R, F>(
    source_file: &File,
    preflight: ZipPreflight,
    archive: &mut ZipArchive<R>,
    limits: CharXLimits,
    cancellation: &Cancellation<F>,
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
    let mut layouts = Vec::with_capacity(archive.len());

    for index in 0..archive.len() {
        cancellation.check()?;
        let entry = archive
            .by_index_raw(index)
            .map_err(|error| structural_zip_error("inspect CharX entry", error))?;
        if !SUPPORTED_COMPRESSION_METHODS.contains(&entry.compression()) {
            return Err(invalid_archive(format!(
                "unsupported CharX compression method: {}",
                entry.compression()
            )));
        }
        let raw_name = entry.name_raw();
        layouts.push(inspect_entry_layout(
            source_file,
            preflight,
            &entry,
            cancellation,
        )?);
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
        let metadata_entry = matches!(normalized_name.as_str(), "card.json" | "module.risum");
        let entry_limit = if metadata_entry {
            limits.max_metadata_bytes
        } else {
            limits.max_entry_decoded_bytes
        };
        if decoded_size > entry_limit {
            return Err(CharXParseError::new(
                if metadata_entry {
                    CharXParseErrorCode::MetadataTooLarge
                } else {
                    CharXParseErrorCode::EntryTooLarge
                },
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

    layouts.sort_unstable_by_key(|layout| layout.local_start);
    for pair in layouts.windows(2) {
        if pair[0].local_end > pair[1].local_start {
            return Err(CharXParseError::new(
                CharXParseErrorCode::InvalidArchive,
                "CharX local entry data extents overlap",
            ));
        }
    }
    layouts.sort_unstable_by_key(|layout| layout.central_start);
    let directory_relative_start = preflight
        .directory_start
        .checked_sub(preflight.archive_start)
        .ok_or_else(|| invalid_archive("CharX directory precedes its archive view"))?;
    let directory_relative_end = directory_relative_start
        .checked_add(preflight.directory_size)
        .ok_or_else(|| invalid_archive("CharX directory extent overflowed"))?;
    if layouts.first().map(|layout| layout.central_start) != Some(directory_relative_start)
        || layouts.last().map(|layout| layout.central_end) != Some(directory_relative_end)
        || layouts
            .windows(2)
            .any(|pair| pair[0].central_end != pair[1].central_start)
    {
        return Err(invalid_archive(
            "CharX central directory extent does not exactly match its entries",
        ));
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

fn inspect_entry_layout<F>(
    source_file: &File,
    preflight: ZipPreflight,
    entry: &zip::read::ZipFile<'_>,
    cancellation: &Cancellation<F>,
) -> Result<EntryLayout, CharXParseError>
where
    F: FnMut() -> bool,
{
    if entry
        .unix_mode()
        .is_some_and(|mode| mode & 0o170000 == 0o120000)
    {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidArchive,
            "CharX archive contains a symbolic-link entry",
        ));
    }

    let archive_directory_start = preflight
        .directory_start
        .checked_sub(preflight.archive_start)
        .ok_or_else(|| invalid_archive("CharX directory precedes its archive view"))?;
    let directory_relative_end = archive_directory_start
        .checked_add(preflight.directory_size)
        .ok_or_else(|| invalid_archive("CharX directory extent overflowed"))?;
    let central_relative_start = entry.central_header_start();
    if central_relative_start < archive_directory_start
        || central_relative_start
            .checked_add(CENTRAL_DIRECTORY_HEADER_BYTES as u64)
            .is_none_or(|end| end > directory_relative_end)
    {
        return Err(invalid_archive(
            "CharX central header exceeds the declared directory",
        ));
    }
    let central_start = preflight
        .archive_start
        .checked_add(central_relative_start)
        .ok_or_else(|| invalid_archive("CharX central header offset overflowed"))?;
    let central = read_exact_at::<CENTRAL_DIRECTORY_HEADER_BYTES, _>(
        source_file,
        central_start,
        cancellation,
        "central ZIP header",
    )?;
    if read_u32(&central, 0) != Some(CENTRAL_DIRECTORY_SIGNATURE) {
        return Err(invalid_archive("invalid central ZIP header signature"));
    }
    let central_flags = read_u16(&central, 8).unwrap();
    let central_method = read_u16(&central, 10).unwrap();
    let central_disk = read_u16(&central, 34).unwrap();
    if central_disk != 0 {
        return Err(invalid_archive("CharX entry belongs to another ZIP disk"));
    }
    let central_name_length = u64::from(read_u16(&central, 28).unwrap());
    let central_extra_length = u64::from(read_u16(&central, 30).unwrap());
    let central_comment_length = u64::from(read_u16(&central, 32).unwrap());
    let central_end = central_relative_start
        .checked_add(CENTRAL_DIRECTORY_HEADER_BYTES as u64)
        .and_then(|value| value.checked_add(central_name_length))
        .and_then(|value| value.checked_add(central_extra_length))
        .and_then(|value| value.checked_add(central_comment_length))
        .ok_or_else(|| invalid_archive("CharX central entry extent overflowed"))?;
    if central_end > directory_relative_end {
        return Err(invalid_archive(
            "CharX central entry exceeds the declared directory",
        ));
    }
    let central_extra_start = central_start
        .checked_add(CENTRAL_DIRECTORY_HEADER_BYTES as u64)
        .and_then(|value| value.checked_add(central_name_length))
        .ok_or_else(|| invalid_archive("CharX central extra offset overflowed"))?;
    let central_extra = read_bytes_at(
        source_file,
        central_extra_start,
        usize::try_from(central_extra_length)
            .map_err(|_| invalid_archive("CharX central extra length does not fit memory"))?,
        cancellation,
        "central ZIP extra data",
    )?;
    reject_aes_extra(&central_extra, "central")?;

    let local_relative_start = entry.header_start();
    if local_relative_start
        .checked_add(LOCAL_FILE_HEADER_BYTES as u64)
        .is_none_or(|end| end > archive_directory_start)
    {
        return Err(invalid_archive(
            "CharX local header enters the central directory",
        ));
    }
    let local_start = preflight
        .archive_start
        .checked_add(local_relative_start)
        .ok_or_else(|| invalid_archive("CharX local header offset overflowed"))?;
    let local = read_exact_at::<LOCAL_FILE_HEADER_BYTES, _>(
        source_file,
        local_start,
        cancellation,
        "local ZIP header",
    )?;
    if read_u32(&local, 0) != Some(LOCAL_FILE_HEADER_SIGNATURE) {
        return Err(invalid_archive("invalid local ZIP header signature"));
    }
    let local_flags = read_u16(&local, 6).unwrap();
    let local_method = read_u16(&local, 8).unwrap();
    if local_flags != central_flags {
        return Err(invalid_archive(
            "CharX local and central general-purpose flags differ",
        ));
    }
    if local_method != central_method {
        return Err(invalid_archive(
            "CharX local and central compression methods differ",
        ));
    }
    if central_method == AES_COMPRESSION_METHOD {
        return Err(invalid_archive(
            "AES-encrypted CharX entries are unsupported",
        ));
    }
    if central_flags & 1 != 0 {
        return Err(invalid_archive("encrypted CharX entries are unsupported"));
    }
    let compression_flags = central_flags & ((1 << 1) | (1 << 2));
    if compression_flags != 0 && central_method != DEFLATE_COMPRESSION_METHOD {
        return Err(invalid_archive(
            "CharX compression-option flags do not apply to this method",
        ));
    }
    const COMMON_SUPPORTED_FLAGS: u16 = (1 << 3) | (1 << 11);
    if central_flags & !(COMMON_SUPPORTED_FLAGS | compression_flags) != 0 {
        return Err(invalid_archive(
            "CharX entry uses unsupported general-purpose flags",
        ));
    }

    let name_length = u64::from(read_u16(&local, 26).unwrap());
    let extra_length = u64::from(read_u16(&local, 28).unwrap());
    let data_start = local_relative_start
        .checked_add(LOCAL_FILE_HEADER_BYTES as u64)
        .and_then(|value| value.checked_add(name_length))
        .and_then(|value| value.checked_add(extra_length))
        .ok_or_else(|| invalid_archive("CharX local header length overflowed"))?;
    if data_start > archive_directory_start {
        return Err(invalid_archive(
            "CharX local name or extra data enters the central directory",
        ));
    }
    let name_start = local_start
        .checked_add(LOCAL_FILE_HEADER_BYTES as u64)
        .ok_or_else(|| invalid_archive("CharX local name offset overflowed"))?;
    let local_name = read_bytes_at(
        source_file,
        name_start,
        usize::from(read_u16(&local, 26).unwrap()),
        cancellation,
        "local ZIP entry name",
    )?;
    if entry.name_raw() != local_name.as_slice() {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidPath,
            "CharX local and central entry names differ",
        ));
    }

    let extra_start = name_start
        .checked_add(name_length)
        .ok_or_else(|| invalid_archive("CharX local extra offset overflowed"))?;
    let local_extra = read_bytes_at(
        source_file,
        extra_start,
        usize::try_from(extra_length)
            .map_err(|_| invalid_archive("CharX local extra length does not fit memory"))?,
        cancellation,
        "local ZIP extra data",
    )?;
    reject_aes_extra(&local_extra, "local")?;

    let data_end = data_start
        .checked_add(entry.compressed_size())
        .ok_or_else(|| invalid_archive("CharX compressed data extent overflowed"))?;
    let local_crc = read_u32(&local, 14).unwrap();
    let local_compressed = read_u32(&local, 18).unwrap();
    let local_decoded = read_u32(&local, 22).unwrap();
    let central_compressed = read_u32(&central, 20).unwrap();
    let central_decoded = read_u32(&central, 24).unwrap();
    validate_local_zip64_sizes(
        &local_extra,
        local_compressed,
        local_decoded,
        entry.compressed_size(),
        entry.size(),
    )?;
    let descriptor_uses_zip64 = local_compressed == u32::MAX
        || local_decoded == u32::MAX
        || central_compressed == u32::MAX
        || central_decoded == u32::MAX
        || entry.compressed_size() > u64::from(u32::MAX)
        || entry.size() > u64::from(u32::MAX);
    let extent_end = if central_flags & (1 << 3) != 0 {
        if !descriptor_placeholder_matches(local_crc, entry.crc32())
            || !descriptor_size_placeholder_matches(local_compressed, entry.compressed_size())
            || !descriptor_size_placeholder_matches(local_decoded, entry.size())
        {
            return Err(invalid_archive(
                "CharX local data-descriptor placeholders are inconsistent",
            ));
        }
        validate_data_descriptor(
            source_file,
            preflight,
            data_end,
            DataDescriptorExpected {
                crc32: entry.crc32(),
                compressed: entry.compressed_size(),
                decoded: entry.size(),
                uses_zip64: descriptor_uses_zip64,
            },
            cancellation,
        )?
    } else {
        if local_crc != entry.crc32()
            || (local_compressed != u32::MAX
                && u64::from(local_compressed) != entry.compressed_size())
            || (local_decoded != u32::MAX && u64::from(local_decoded) != entry.size())
        {
            return Err(invalid_archive(
                "CharX local and central CRC or sizes differ",
            ));
        }
        data_end
    };

    if local_relative_start >= archive_directory_start || extent_end > archive_directory_start {
        return Err(invalid_archive(
            "CharX local entry data enters the central directory",
        ));
    }
    Ok(EntryLayout {
        local_start: local_relative_start,
        local_end: extent_end,
        central_start: central_relative_start,
        central_end,
    })
}

fn validate_local_zip64_sizes(
    extra: &[u8],
    local_compressed: u32,
    local_decoded: u32,
    expected_compressed: u64,
    expected_decoded: u64,
) -> Result<(), CharXParseError> {
    if local_compressed != u32::MAX && local_decoded != u32::MAX {
        return Ok(());
    }

    let values = find_extra_field(extra, ZIP64_EXTRA_FIELD_KIND, "local")?
        .ok_or_else(|| invalid_archive("CharX local ZIP64 sizes have no ZIP64 extra field"))?;
    if values.len() < 16
        || read_u64(values, 0) != Some(expected_decoded)
        || read_u64(values, 8) != Some(expected_compressed)
    {
        return Err(invalid_archive(
            "CharX local ZIP64 size values differ from the central directory",
        ));
    }
    Ok(())
}

fn reject_aes_extra(extra: &[u8], location: &str) -> Result<(), CharXParseError> {
    if find_extra_field(extra, AES_EXTRA_FIELD_KIND, location)?.is_some() {
        return Err(invalid_archive(format!(
            "CharX {location} header contains unsupported AES metadata"
        )));
    }
    Ok(())
}

fn find_extra_field<'a>(
    extra: &'a [u8],
    target_kind: u16,
    location: &str,
) -> Result<Option<&'a [u8]>, CharXParseError> {
    let mut offset = 0_usize;
    let mut target = None;
    while offset < extra.len() {
        let kind = read_u16(extra, offset).ok_or_else(|| {
            invalid_archive(format!("CharX {location} extra header is truncated"))
        })?;
        let length = usize::from(read_u16(extra, offset + 2).ok_or_else(|| {
            invalid_archive(format!("CharX {location} extra header is truncated"))
        })?);
        let value_start = offset
            .checked_add(4)
            .ok_or_else(|| invalid_archive(format!("CharX {location} extra offset overflowed")))?;
        let value_end = value_start
            .checked_add(length)
            .filter(|end| *end <= extra.len())
            .ok_or_else(|| invalid_archive(format!("CharX {location} extra is truncated")))?;
        if kind == target_kind && target.replace(&extra[value_start..value_end]).is_some() {
            return Err(invalid_archive(format!(
                "CharX {location} header has duplicate extra field {target_kind:#06x}"
            )));
        }
        offset = value_end;
    }
    Ok(target)
}

fn descriptor_placeholder_matches(value: u32, expected: u32) -> bool {
    value == 0 || value == expected
}

fn descriptor_size_placeholder_matches(value: u32, expected: u64) -> bool {
    value == 0 || value == u32::MAX || u64::from(value) == expected
}

fn validate_data_descriptor<F>(
    source_file: &File,
    preflight: ZipPreflight,
    data_end: u64,
    expected: DataDescriptorExpected,
    cancellation: &Cancellation<F>,
) -> Result<u64, CharXParseError>
where
    F: FnMut() -> bool,
{
    let directory_start = preflight
        .directory_start
        .checked_sub(preflight.archive_start)
        .ok_or_else(|| invalid_archive("CharX directory precedes its archive view"))?;
    let available = directory_start
        .checked_sub(data_end)
        .ok_or_else(|| invalid_archive("CharX data descriptor enters the central directory"))?
        .min(24);
    if available < 12 {
        return Err(invalid_archive("CharX data descriptor is truncated"));
    }
    let descriptor_start = preflight
        .archive_start
        .checked_add(data_end)
        .ok_or_else(|| invalid_archive("CharX data descriptor offset overflowed"))?;
    let bytes = read_bytes_at(
        source_file,
        descriptor_start,
        available as usize,
        cancellation,
        "ZIP data descriptor",
    )?;

    let mut offsets = [None, None];
    offsets[0] = Some(0_usize);
    if read_u32(&bytes, 0) == Some(DATA_DESCRIPTOR_SIGNATURE) {
        offsets[1] = Some(4);
    }
    for offset in offsets.into_iter().flatten() {
        let payload_bytes = if expected.uses_zip64 { 20 } else { 12 };
        let matches = if expected.uses_zip64 {
            bytes.len() >= offset + payload_bytes
                && read_u32(&bytes, offset) == Some(expected.crc32)
                && read_u64(&bytes, offset + 4) == Some(expected.compressed)
                && read_u64(&bytes, offset + 12) == Some(expected.decoded)
        } else {
            bytes.len() >= offset + payload_bytes
                && read_u32(&bytes, offset) == Some(expected.crc32)
                && read_u32(&bytes, offset + 4).map(u64::from) == Some(expected.compressed)
                && read_u32(&bytes, offset + 8).map(u64::from) == Some(expected.decoded)
        };
        if matches {
            return data_end
                .checked_add((offset + payload_bytes) as u64)
                .ok_or_else(|| invalid_archive("CharX data descriptor extent overflowed"));
        }
    }

    Err(invalid_archive(
        "CharX data descriptor differs from the central directory",
    ))
}

fn read_exact_at<const N: usize, F>(
    source_file: &File,
    offset: u64,
    cancellation: &Cancellation<F>,
    label: &str,
) -> Result<[u8; N], CharXParseError>
where
    F: FnMut() -> bool,
{
    let bytes = read_bytes_at(source_file, offset, N, cancellation, label)?;
    Ok(bytes.try_into().expect("read exact fixed-size buffer"))
}

fn read_bytes_at<F>(
    source_file: &File,
    offset: u64,
    length: usize,
    cancellation: &Cancellation<F>,
    label: &str,
) -> Result<Vec<u8>, CharXParseError>
where
    F: FnMut() -> bool,
{
    let mut bytes = vec![0_u8; length];
    let mut completed = 0;
    while completed < length {
        cancellation.check()?;
        #[cfg(windows)]
        let saved_position = (&*source_file)
            .stream_position()
            .map_err(|e| io_error(label, e))?;
        #[cfg(windows)]
        let read = std::os::windows::fs::FileExt::seek_read(
            source_file,
            &mut bytes[completed..],
            offset + completed as u64,
        );
        #[cfg(windows)]
        (&*source_file)
            .seek(SeekFrom::Start(saved_position))
            .map_err(|e| io_error(label, e))?;
        #[cfg(unix)]
        let read = std::os::unix::fs::FileExt::read_at(
            source_file,
            &mut bytes[completed..],
            offset + completed as u64,
        );
        let read = read.map_err(|error| io_error(label, error))?;
        if read == 0 {
            return Err(invalid_archive(format!("truncated {label}")));
        }
        completed += read;
    }
    Ok(bytes)
}

fn invalid_archive(message: impl Into<String>) -> CharXParseError {
    CharXParseError::new(CharXParseErrorCode::InvalidArchive, message)
}

fn read_entry_to_memory<R, F>(
    archive: &mut ZipArchive<R>,
    entry: &EntryMetadata,
    byte_limit: u64,
    aggregate_actual: &mut u64,
    aggregate_limit: u64,
    cancellation: &Cancellation<F>,
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
        cancellation,
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
    cancellation: &Cancellation<F>,
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
        cancellation,
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
    cancellation: &Cancellation<F>,
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
        cancellation.check()?;
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
    entries: &[EntryMetadata],
) -> Result<Vec<CharXAssetReference>, CharXParseError> {
    let card: Value = serde_json::from_str(card_json).map_err(|error| {
        CharXParseError::new(
            CharXParseErrorCode::InvalidCardMetadata,
            format!("card.json is invalid JSON: {error}"),
        )
    })?;
    if card.get("spec").and_then(Value::as_str) != Some("chara_card_v3") {
        return Err(CharXParseError::new(
            CharXParseErrorCode::InvalidCardMetadata,
            "card.json is not a Character Card V3 object",
        ));
    }
    let data = card
        .get("data")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_card("card.json data must be an object"))?;
    let extensions = data
        .get("extensions")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid_card("card.json data.extensions must be an object"))?;
    if extensions
        .get("risuai")
        .is_some_and(|risuai| !risuai.is_object())
    {
        return Err(invalid_card(
            "card.json data.extensions.risuai must be an object when present",
        ));
    }

    let payload_names: HashSet<&str> = entries
        .iter()
        .filter(|entry| !entry.is_directory && entry.normalized_name != "card.json")
        .map(|entry| entry.normalized_name.as_str())
        .collect();
    let mut references = Vec::new();
    let assets = match data.get("assets") {
        None => return Ok(references),
        Some(assets) => assets
            .as_array()
            .ok_or_else(|| invalid_card("card.json data.assets must be an array"))?,
    };

    for (order, asset) in assets.iter().enumerate() {
        let asset = asset
            .as_object()
            .ok_or_else(|| invalid_card(format!("card.json asset {order} must be an object")))?;
        let asset_type = required_asset_string(asset, "type", order)?;
        let uri = required_asset_string(asset, "uri", order)?;
        let display_name = required_asset_string(asset, "name", order)?;
        let declared_extension = required_asset_string(asset, "ext", order)?;

        let referenced_name = uri
            .strip_prefix("embeded://")
            .or_else(|| uri.strip_prefix("__asset:"));
        let Some(original_name) = referenced_name else {
            validate_non_archive_asset_uri(uri, order)?;
            continue;
        };
        if original_name.is_empty() {
            return Err(invalid_card(format!(
                "card.json asset {order} has an empty archive reference"
            )));
        }
        let (normalized_name, _) = validate_archive_name(original_name, false).map_err(|_| {
            invalid_card(format!(
                "card.json contains an unsafe archive asset URI: {uri}"
            ))
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
            asset_type: asset_type.to_owned(),
            display_name: Some(display_name.to_owned()),
            declared_extension: Some(declared_extension.to_owned()),
        });
    }
    Ok(references)
}

fn required_asset_string<'a>(
    asset: &'a serde_json::Map<String, Value>,
    field: &str,
    order: usize,
) -> Result<&'a str, CharXParseError> {
    asset.get(field).and_then(Value::as_str).ok_or_else(|| {
        invalid_card(format!(
            "card.json asset {order} field {field} must be a string"
        ))
    })
}

fn validate_non_archive_asset_uri(uri: &str, order: usize) -> Result<(), CharXParseError> {
    if uri == "ccdefault:" {
        return Ok(());
    }
    if uri.starts_with("ccdefault:") || uri.starts_with("embeded:") || uri.starts_with("__asset:") {
        return Err(invalid_card(format!(
            "card.json asset {order} has a malformed built-in URI"
        )));
    }
    if uri.starts_with("data:") {
        return validate_data_uri(uri, order);
    }
    Url::parse(uri)
        .ok()
        .filter(|parsed| !parsed.scheme().is_empty())
        .ok_or_else(|| invalid_card(format!("card.json asset {order} has an invalid URI")))
        .map(|_| ())
}

fn validate_data_uri(uri: &str, order: usize) -> Result<(), CharXParseError> {
    let (metadata, payload) = uri
        .strip_prefix("data:")
        .and_then(|value| value.split_once(','))
        .ok_or_else(|| invalid_card(format!("card.json asset {order} has an invalid data URI")))?;
    let mut fields = metadata.split(';');
    let media_type = fields.next().unwrap_or_default();
    let parameters: Vec<&str> = fields.collect();
    if !valid_media_type(media_type)
        || parameters.last().copied() != Some("base64")
        || BASE64_STANDARD.decode(payload).is_err()
    {
        return Err(invalid_card(format!(
            "card.json asset {order} has an invalid base64 data URI"
        )));
    }
    Ok(())
}

fn valid_media_type(media_type: &str) -> bool {
    let Some((major, minor)) = media_type.split_once('/') else {
        return false;
    };
    !major.is_empty()
        && !minor.is_empty()
        && major.bytes().chain(minor.bytes()).all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'!' | b'#'..=b'+' | b'-' | b'.' | b'^'..=b'~')
        })
}

fn invalid_card(message: impl Into<String>) -> CharXParseError {
    CharXParseError::new(CharXParseErrorCode::InvalidCardMetadata, message)
}

fn extension_of(name: &str) -> Option<String> {
    let final_segment = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let separator = final_segment.rfind('.')?;
    if separator == 0 || separator + 1 == final_segment.len() {
        return None;
    }
    Some(final_segment[separator + 1..].to_owned())
}

fn has_jpeg_signature<F>(
    file: &File,
    cancellation: &Cancellation<F>,
) -> Result<bool, CharXParseError>
where
    F: FnMut() -> bool,
{
    if file
        .metadata()
        .map_err(|e| io_error("source length", e))?
        .len()
        < 3
    {
        return Ok(false);
    }
    Ok(read_bytes_at(file, 0, 3, cancellation, "JPEG signature")? == [0xff, 0xd8, 0xff])
}

fn jpeg_prefix_ends_at_archive<F>(
    file: &File,
    archive_offset: u64,
    cancellation: &Cancellation<F>,
) -> Result<bool, CharXParseError>
where
    F: FnMut() -> bool,
{
    if archive_offset < 2 {
        return Ok(false);
    }
    Ok(
        read_bytes_at(file, archive_offset - 2, 2, cancellation, "JPEG end marker")?
            == [0xff, 0xd9],
    )
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

fn io_error(context: &str, error: io::Error) -> CharXParseError {
    if error.to_string() == CANCELLED_IO_MESSAGE {
        return CharXParseError::new(CharXParseErrorCode::Cancelled, CANCELLED_IO_MESSAGE);
    }
    CharXParseError::new(CharXParseErrorCode::Io, format!("{context}: {error}"))
}

fn zip_error(context: &str, error: zip::result::ZipError) -> CharXParseError {
    match error {
        zip::result::ZipError::Io(error) => io_error(context, error),
        error => CharXParseError::new(
            CharXParseErrorCode::InvalidArchive,
            format!("{context}: {error}"),
        ),
    }
}

fn structural_zip_error(context: &str, error: zip::result::ZipError) -> CharXParseError {
    match error {
        zip::result::ZipError::Io(error) if error.to_string() == CANCELLED_IO_MESSAGE => {
            CharXParseError::new(CharXParseErrorCode::Cancelled, CANCELLED_IO_MESSAGE)
        }
        zip::result::ZipError::Io(error)
            if matches!(
                error.kind(),
                io::ErrorKind::InvalidInput | io::ErrorKind::UnexpectedEof
            ) =>
        {
            invalid_archive(format!("{context}: {error}"))
        }
        error => zip_error(context, error),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        validate_data_descriptor, Cancellation, DataDescriptorExpected, ZipPreflight,
        DATA_DESCRIPTOR_SIGNATURE,
    };
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn unsigned_descriptor_crc_that_equals_the_optional_signature_is_not_misclassified() {
        let directory = TempDir::new().expect("temporary directory");
        let source = directory.path().join("descriptor.bin");
        let mut descriptor = Vec::new();
        descriptor.extend_from_slice(&DATA_DESCRIPTOR_SIGNATURE.to_le_bytes());
        descriptor.extend_from_slice(&7_u32.to_le_bytes());
        descriptor.extend_from_slice(&9_u32.to_le_bytes());
        fs::write(&source, descriptor).expect("write descriptor fixture");
        let cancellation = Cancellation::new(|| false);
        let preflight = ZipPreflight {
            entry_count: 1,
            archive_start: 0,
            archive_end: 12,
            directory_start: 12,
            directory_size: 0,
        };

        assert_eq!(
            validate_data_descriptor(
                &fs::File::open(&source).unwrap(),
                preflight,
                0,
                DataDescriptorExpected {
                    crc32: DATA_DESCRIPTOR_SIGNATURE,
                    compressed: 7,
                    decoded: 9,
                    uses_zip64: false,
                },
                &cancellation,
            )
            .expect("unsigned descriptor must match at offset zero"),
            12
        );
    }
}
