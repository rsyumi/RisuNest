use super::{FormatError, ImportLimits};
use serde_json::Value;
use std::io::{self, Read, Seek, SeekFrom};

const SNIFF_PREFIX_BYTES: usize = 16;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const ZIP_PREFLIGHT_READ_BYTES: usize = 64 * 1024;
const ZIP_COMMENT_MAX_BYTES: usize = u16::MAX as usize;
const ZIP_EOCD_BYTES: usize = 22;
const ZIP64_LOCATOR_BYTES: usize = 20;
const ZIP64_EOCD_MIN_BYTES: usize = 56;
const ZIP_PREFLIGHT_TAIL_BYTES: usize =
    ZIP_COMMENT_MAX_BYTES + ZIP_EOCD_BYTES + ZIP64_LOCATOR_BYTES + ZIP64_EOCD_MIN_BYTES;
const ZIP_EOCD_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
const ZIP64_LOCATOR_SIGNATURE: &[u8; 4] = b"PK\x06\x07";
const ZIP64_EOCD_SIGNATURE: &[u8; 4] = b"PK\x06\x06";
const ZIP_CENTRAL_ENTRY_SIGNATURE: &[u8; 4] = b"PK\x01\x02";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentKind {
    RisuModule,
    JsonCard,
    PngCard,
    CharxCard,
    AppendedCharxJpeg,
    JpegAsset,
    Unknown,
}

pub fn classify_content(
    file_name: &str,
    reader: &mut (impl Read + Seek),
    limits: &ImportLimits,
    cancelled: &impl Fn() -> bool,
) -> Result<ContentKind, FormatError> {
    if cancelled() {
        return Err(FormatError::cancelled());
    }
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| FormatError::io("seek import source", error))?;
    let mut prefix = [0_u8; SNIFF_PREFIX_BYTES];
    let prefix_length = read_prefix(reader, &mut prefix, cancelled)?;
    let prefix = &prefix[..prefix_length];

    if prefix.starts_with(&[0xff, 0xd8, 0xff]) {
        let appended = probe_charx(
            reader,
            limits.charx_probe_metadata_bytes,
            limits.max_container_entries,
            limits.max_container_directory_bytes,
            cancelled,
        )?;
        return Ok(if appended {
            ContentKind::AppendedCharxJpeg
        } else {
            ContentKind::JpegAsset
        });
    }
    if prefix.starts_with(&[111, 0]) {
        return Ok(ContentKind::RisuModule);
    }
    if prefix.starts_with(PNG_SIGNATURE) {
        return Ok(ContentKind::PngCard);
    }
    if looks_like_json(prefix) {
        return Ok(ContentKind::JsonCard);
    }
    if probe_charx(
        reader,
        limits.charx_probe_metadata_bytes,
        limits.max_container_entries,
        limits.max_container_directory_bytes,
        cancelled,
    )? {
        return Ok(ContentKind::CharxCard);
    }

    Ok(match extension(file_name).as_deref() {
        Some("risum") => ContentKind::RisuModule,
        Some("json") => ContentKind::JsonCard,
        Some("jpg") | Some("jpeg") => ContentKind::JpegAsset,
        _ => ContentKind::Unknown,
    })
}

fn probe_charx(
    reader: &mut (impl Read + Seek),
    metadata_limit: u64,
    max_entries: usize,
    max_directory_bytes: u64,
    cancelled: &impl Fn() -> bool,
) -> Result<bool, FormatError> {
    if cancelled() {
        return Err(FormatError::cancelled());
    }
    let Some(preflight) = preflight_zip(reader, max_entries, max_directory_bytes, cancelled)?
    else {
        return Ok(false);
    };
    if cancelled() {
        return Err(FormatError::cancelled());
    }
    let view = ArchiveView::new(reader, preflight.archive_start, preflight.archive_end)?;
    let mut archive = match zip::ZipArchive::new(view) {
        Ok(archive) => archive,
        Err(_) => return Ok(false),
    };
    if cancelled() {
        return Err(FormatError::cancelled());
    }
    if archive.offset() != 0 || archive.len() != preflight.entry_count {
        return Ok(false);
    }
    let mut card_index = None;
    for index in 0..archive.len() {
        if cancelled() {
            return Err(FormatError::cancelled());
        }
        let entry = match archive.by_index(index) {
            Ok(entry) => entry,
            Err(_) => return Ok(false),
        };
        if entry.name() == "card.json" && card_index.replace(index).is_some() {
            return Ok(false);
        }
    }
    let Some(card_index) = card_index else {
        return Ok(false);
    };
    let mut card = match archive.by_index(card_index) {
        Ok(card) => card,
        Err(_) => return Ok(false),
    };
    if card.size() > metadata_limit {
        return Ok(false);
    }
    let expected_size = card.size();
    let capacity = match usize::try_from(expected_size) {
        Ok(capacity) => capacity,
        Err(_) => return Ok(false),
    };
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if cancelled() {
            return Err(FormatError::cancelled());
        }
        let read = match card.read(&mut buffer) {
            Ok(read) => read,
            Err(_) => return Ok(false),
        };
        if read == 0 {
            break;
        }
        if (bytes.len() as u64)
            .checked_add(read as u64)
            .is_none_or(|length| length > metadata_limit)
        {
            return Ok(false);
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.len() as u64 != expected_size {
        return Ok(false);
    }
    if cancelled() {
        return Err(FormatError::cancelled());
    }
    let Ok(value) = serde_json::from_slice::<Value>(&bytes) else {
        return Ok(false);
    };
    Ok(
        value.get("spec").and_then(Value::as_str) == Some("chara_card_v3")
            && value.get("data").is_some_and(Value::is_object),
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ZipPreflight {
    pub(crate) entry_count: usize,
    pub(crate) archive_start: u64,
    pub(crate) archive_end: u64,
    pub(crate) directory_start: u64,
    pub(crate) directory_size: u64,
}

pub(crate) fn preflight_zip(
    reader: &mut (impl Read + Seek),
    max_entries: usize,
    max_directory_bytes: u64,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<ZipPreflight>, FormatError> {
    if cancelled() {
        return Err(FormatError::cancelled());
    }
    let file_length = reader
        .seek(SeekFrom::End(0))
        .map_err(|error| FormatError::io("seek ZIP preflight end", error))?;
    if file_length < ZIP_EOCD_BYTES as u64 {
        return Ok(None);
    }
    let tail_length = file_length.min(ZIP_PREFLIGHT_TAIL_BYTES as u64) as usize;
    let tail_start = file_length - tail_length as u64;
    reader
        .seek(SeekFrom::Start(tail_start))
        .map_err(|error| FormatError::io("seek ZIP preflight tail", error))?;
    let mut tail = vec![0_u8; tail_length];
    if !read_preflight_exact(reader, &mut tail, cancelled)? {
        return Ok(None);
    }

    let Some(eocd_index) = find_eocd(&tail) else {
        return Ok(None);
    };
    let eocd_absolute = tail_start
        .checked_add(eocd_index as u64)
        .ok_or_else(|| FormatError::invalid("ZIP EOCD offset overflow"))?;
    let disk_number = le_u16(&tail, eocd_index + 4)?;
    let central_disk = le_u16(&tail, eocd_index + 6)?;
    let disk_entries = le_u16(&tail, eocd_index + 8)?;
    let total_entries = le_u16(&tail, eocd_index + 10)?;
    let directory_size = le_u32(&tail, eocd_index + 12)?;
    let directory_offset = le_u32(&tail, eocd_index + 16)?;
    let comment_length = le_u16(&tail, eocd_index + 20)? as u64;
    let Some(archive_end) = eocd_absolute
        .checked_add(ZIP_EOCD_BYTES as u64)
        .and_then(|end| end.checked_add(comment_length))
    else {
        return Ok(None);
    };
    if archive_end > file_length {
        return Ok(None);
    }

    let locator_index = eocd_index.checked_sub(ZIP64_LOCATOR_BYTES);
    if locator_index
        .is_some_and(|index| tail.get(index..index + 4) == Some(ZIP64_LOCATOR_SIGNATURE))
    {
        return preflight_zip64(
            reader,
            &tail,
            tail_start,
            eocd_index,
            archive_end,
            max_entries,
            max_directory_bytes,
            cancelled,
        );
    }
    if disk_number != 0 || central_disk != 0 {
        return Ok(None);
    }
    if disk_entries != total_entries {
        return Ok(None);
    }
    let entry_count = total_entries as usize;
    if entry_count > max_entries || directory_size as u64 > max_directory_bytes {
        return Ok(None);
    }
    let Some((archive_start, directory_start)) = validate_directory_bounds(
        reader,
        eocd_absolute,
        directory_size as u64,
        directory_offset as u64,
        entry_count,
        cancelled,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(ZipPreflight {
        entry_count,
        archive_start,
        archive_end,
        directory_start,
        directory_size: directory_size as u64,
    }))
}

#[allow(clippy::too_many_arguments)]
fn preflight_zip64(
    reader: &mut (impl Read + Seek),
    tail: &[u8],
    tail_start: u64,
    eocd_index: usize,
    archive_end: u64,
    max_entries: usize,
    max_directory_bytes: u64,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<ZipPreflight>, FormatError> {
    let Some(locator_index) = eocd_index.checked_sub(ZIP64_LOCATOR_BYTES) else {
        return Ok(None);
    };
    if tail.get(locator_index..locator_index + 4) != Some(ZIP64_LOCATOR_SIGNATURE) {
        return Ok(None);
    }
    let locator_disk = le_u32(tail, locator_index + 4)?;
    let locator_record_offset = le_u64(tail, locator_index + 8)?;
    let locator_disks = le_u32(tail, locator_index + 16)?;
    if locator_disk != 0 || locator_disks != 1 {
        return Ok(None);
    }

    let mut record_index = None;
    for candidate in (0..locator_index).rev() {
        if tail.get(candidate..candidate + 4) != Some(ZIP64_EOCD_SIGNATURE) {
            continue;
        }
        let Some(record_size) = optional_le_u64(tail, candidate + 4) else {
            continue;
        };
        let Some(record_end) = (candidate as u64)
            .checked_add(12)
            .and_then(|value| value.checked_add(record_size))
        else {
            continue;
        };
        if record_size >= 44 && record_end == locator_index as u64 {
            record_index = Some(candidate);
            break;
        }
    }
    let Some(record_index) = record_index else {
        return Ok(None);
    };
    let record_absolute = tail_start
        .checked_add(record_index as u64)
        .ok_or_else(|| FormatError::invalid("ZIP64 EOCD offset overflow"))?;
    let disk_number = le_u32(tail, record_index + 16)?;
    let central_disk = le_u32(tail, record_index + 20)?;
    let disk_entries = le_u64(tail, record_index + 24)?;
    let total_entries = le_u64(tail, record_index + 32)?;
    let directory_size = le_u64(tail, record_index + 40)?;
    let directory_offset = le_u64(tail, record_index + 48)?;
    if disk_number != 0 || central_disk != 0 || disk_entries != total_entries {
        return Ok(None);
    }
    let Ok(entry_count) = usize::try_from(total_entries) else {
        return Ok(None);
    };
    if entry_count > max_entries || directory_size > max_directory_bytes {
        return Ok(None);
    }
    let Some(directory_start) = record_absolute.checked_sub(directory_size) else {
        return Ok(None);
    };
    let Some(archive_start) = directory_start.checked_sub(directory_offset) else {
        return Ok(None);
    };
    if locator_record_offset
        .checked_add(archive_start)
        .is_none_or(|offset| offset != record_absolute)
    {
        return Ok(None);
    }
    if !validate_directory_signature(reader, directory_start, entry_count, cancelled)? {
        return Ok(None);
    }
    Ok(Some(ZipPreflight {
        entry_count,
        archive_start,
        archive_end,
        directory_start,
        directory_size,
    }))
}

fn validate_directory_bounds(
    reader: &mut (impl Read + Seek),
    directory_end: u64,
    directory_size: u64,
    directory_offset: u64,
    entry_count: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<Option<(u64, u64)>, FormatError> {
    let Some(directory_start) = directory_end.checked_sub(directory_size) else {
        return Ok(None);
    };
    let Some(archive_start) = directory_start.checked_sub(directory_offset) else {
        return Ok(None);
    };
    Ok(
        validate_directory_signature(reader, directory_start, entry_count, cancelled)?
            .then_some((archive_start, directory_start)),
    )
}

pub(crate) struct ArchiveView<'a, R> {
    reader: &'a mut R,
    start: u64,
    length: u64,
    position: u64,
}

impl<'a, R: Read + Seek> ArchiveView<'a, R> {
    pub(crate) fn new(reader: &'a mut R, start: u64, end: u64) -> Result<Self, FormatError> {
        let length = end
            .checked_sub(start)
            .ok_or_else(|| FormatError::invalid("ZIP archive bounds are reversed"))?;
        reader
            .seek(SeekFrom::Start(start))
            .map_err(|error| FormatError::io("seek bounded ZIP archive", error))?;
        Ok(Self {
            reader,
            start,
            length,
            position: 0,
        })
    }
}

impl<R: Read> Read for ArchiveView<'_, R> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let remaining = self.length.saturating_sub(self.position);
        if remaining == 0 || output.is_empty() {
            return Ok(0);
        }
        let allowed = usize::try_from(remaining.min(output.len() as u64)).unwrap_or(output.len());
        let read = self.reader.read(&mut output[..allowed])?;
        self.position = self
            .position
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ZIP read overflow"))?;
        Ok(read)
    }
}

impl<R: Seek> Seek for ArchiveView<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let target = match position {
            SeekFrom::Start(offset) => offset as i128,
            SeekFrom::End(offset) => self.length as i128 + offset as i128,
            SeekFrom::Current(offset) => self.position as i128 + offset as i128,
        };
        let target = u64::try_from(target)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid ZIP seek"))?;
        if target > self.length {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ZIP seek exceeds bounded archive",
            ));
        }
        let absolute = self
            .start
            .checked_add(target)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ZIP seek overflow"))?;
        self.reader.seek(SeekFrom::Start(absolute))?;
        self.position = target;
        Ok(target)
    }
}

fn validate_directory_signature(
    reader: &mut (impl Read + Seek),
    directory_start: u64,
    entry_count: usize,
    cancelled: &impl Fn() -> bool,
) -> Result<bool, FormatError> {
    if entry_count == 0 {
        return Ok(true);
    }
    if cancelled() {
        return Err(FormatError::cancelled());
    }
    reader
        .seek(SeekFrom::Start(directory_start))
        .map_err(|error| FormatError::io("seek ZIP central directory", error))?;
    let mut signature = [0_u8; 4];
    if !read_preflight_exact(reader, &mut signature, cancelled)? {
        return Ok(false);
    }
    Ok(&signature == ZIP_CENTRAL_ENTRY_SIGNATURE)
}

fn read_prefix(
    reader: &mut impl Read,
    prefix: &mut [u8],
    cancelled: &impl Fn() -> bool,
) -> Result<usize, FormatError> {
    let mut filled = 0;
    while filled < prefix.len() {
        if cancelled() {
            return Err(FormatError::cancelled());
        }
        let read = reader
            .read(&mut prefix[filled..])
            .map_err(|error| FormatError::io("read import source prefix", error))?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    if cancelled() {
        return Err(FormatError::cancelled());
    }
    Ok(filled)
}

fn read_preflight_exact(
    reader: &mut impl Read,
    output: &mut [u8],
    cancelled: &impl Fn() -> bool,
) -> Result<bool, FormatError> {
    let mut offset = 0;
    while offset < output.len() {
        if cancelled() {
            return Err(FormatError::cancelled());
        }
        let end = offset
            .checked_add(ZIP_PREFLIGHT_READ_BYTES)
            .unwrap_or(output.len())
            .min(output.len());
        let read = reader
            .read(&mut output[offset..end])
            .map_err(|error| FormatError::io("read ZIP preflight", error))?;
        if read == 0 {
            return Ok(false);
        }
        offset += read;
    }
    if cancelled() {
        return Err(FormatError::cancelled());
    }
    Ok(true)
}

fn find_eocd(tail: &[u8]) -> Option<usize> {
    if tail.len() < ZIP_EOCD_BYTES {
        return None;
    }
    let search_start = tail
        .len()
        .saturating_sub(ZIP_EOCD_BYTES + ZIP_COMMENT_MAX_BYTES);
    (search_start..=tail.len() - ZIP_EOCD_BYTES)
        .rev()
        .find(|index| tail.get(*index..*index + 4) == Some(ZIP_EOCD_SIGNATURE))
}

fn le_u16(bytes: &[u8], offset: usize) -> Result<u16, FormatError> {
    optional_le_u16(bytes, offset)
        .ok_or_else(|| FormatError::invalid("truncated ZIP preflight integer"))
}

fn le_u32(bytes: &[u8], offset: usize) -> Result<u32, FormatError> {
    bytes
        .get(offset..offset + 4)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| FormatError::invalid("truncated ZIP preflight integer"))
}

fn le_u64(bytes: &[u8], offset: usize) -> Result<u64, FormatError> {
    optional_le_u64(bytes, offset)
        .ok_or_else(|| FormatError::invalid("truncated ZIP preflight integer"))
}

fn optional_le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset + 2)
        .and_then(|value| value.try_into().ok())
        .map(u16::from_le_bytes)
}

fn optional_le_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    bytes
        .get(offset..offset + 8)
        .and_then(|value| value.try_into().ok())
        .map(u64::from_le_bytes)
}

fn looks_like_json(prefix: &[u8]) -> bool {
    matches!(
        prefix
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace()),
        Some(b'{')
    )
}

fn extension(file_name: &str) -> Option<String> {
    std::path::Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}
