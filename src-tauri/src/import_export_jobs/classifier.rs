use super::{FormatError, ImportLimits};
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentKind {
    RisuModule,
    JsonCard,
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
    let mut prefix = [0_u8; 16];
    let prefix_length = reader
        .read(&mut prefix)
        .map_err(|error| FormatError::io("read import source prefix", error))?;
    let prefix = &prefix[..prefix_length];

    if prefix.starts_with(&[0xff, 0xd8, 0xff]) {
        let appended = probe_charx(
            reader,
            limits.charx_probe_metadata_bytes,
            limits.max_container_entries,
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
    if looks_like_json(prefix) {
        return Ok(ContentKind::JsonCard);
    }
    if probe_charx(
        reader,
        limits.charx_probe_metadata_bytes,
        limits.max_container_entries,
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
    cancelled: &impl Fn() -> bool,
) -> Result<bool, FormatError> {
    if cancelled() {
        return Err(FormatError::cancelled());
    }
    reader
        .seek(SeekFrom::Start(0))
        .map_err(|error| FormatError::io("seek CharX probe source", error))?;
    let mut archive = match zip::ZipArchive::new(reader) {
        Ok(archive) => archive,
        Err(_) => return Ok(false),
    };
    if archive.len() > max_entries {
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
        if entry.name() == "card.json" {
            if card_index.replace(index).is_some() {
                return Ok(false);
            }
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
