//! Reads an earlier character or module file as the base of a content update.
//! Only the card and module metadata are read; asset entries, chunks and
//! frames are skipped, and the WebView interprets what is returned.

use super::{
    classifier::{preflight_zip, ArchiveView},
    decode_rpack, FormatError, MAX_CONTENT_METADATA_BYTES,
};
use std::io::{Read, Seek, SeekFrom};

const PNG_SIGNATURE_BYTES: u64 = 8;
/// The importer looks for a text chunk's keyword terminator within this many bytes.
const TEXT_KEYWORD_SCAN_BYTES: u64 = 70;
const MAX_ZIP_ENTRIES: usize = 50_000;
const MAX_ZIP_DIRECTORY_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpdateBaseFormat {
    Json,
    Png,
    Charx,
    Risum,
}

impl UpdateBaseFormat {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "json" => Some(Self::Json),
            "png" => Some(Self::Png),
            "charx" => Some(Self::Charx),
            "risum" => Some(Self::Risum),
            _ => None,
        }
    }
}

/// The tag of each part in the reply, which is followed by a little-endian
/// u32 length and the part's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum Part {
    /// A whole JSON or lorebook file.
    File = 1,
    /// The card.json entry of a CHARX.
    Card = 2,
    /// The decoded metadata of a RISUM file, alone or inside a CHARX.
    Module = 3,
    /// The `ccv3` and `chara` text chunks of a PNG, still in base64.
    Ccv3 = 4,
    Chara = 5,
}

#[derive(Default)]
struct Parts(Vec<u8>);

impl Parts {
    fn push(&mut self, part: Part, bytes: &[u8]) -> Result<(), FormatError> {
        let length = u32::try_from(bytes.len())
            .map_err(|_| FormatError::limit("update base part exceeds 4 GiB"))?;
        self.0.push(part as u8);
        self.0.extend_from_slice(&length.to_le_bytes());
        self.0.extend_from_slice(bytes);
        Ok(())
    }
}

pub(crate) fn read_update_base(
    source: &mut (impl Read + Seek),
    format: UpdateBaseFormat,
) -> Result<Vec<u8>, FormatError> {
    read_update_base_within(source, format, MAX_CONTENT_METADATA_BYTES as u64)
}

fn read_update_base_within(
    source: &mut (impl Read + Seek),
    format: UpdateBaseFormat,
    limit: u64,
) -> Result<Vec<u8>, FormatError> {
    let mut parts = Parts::default();
    match format {
        UpdateBaseFormat::Json => parts.push(Part::File, &read_bounded(source, limit, "update base file")?)?,
        UpdateBaseFormat::Png => read_png_text(source, limit, &mut parts)?,
        UpdateBaseFormat::Charx => read_charx(source, limit, &mut parts)?,
        UpdateBaseFormat::Risum => parts.push(Part::Module, &read_risum_metadata(source, limit)?)?,
    }
    Ok(parts.0)
}

fn read_bounded(reader: &mut impl Read, limit: u64, label: &str) -> Result<Vec<u8>, FormatError> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| FormatError::io(&format!("read {label}"), error))?;
    if bytes.len() as u64 > limit {
        return Err(FormatError::limit(format!("{label} exceeds the metadata limit")));
    }
    Ok(bytes)
}

fn read_png_text(
    source: &mut (impl Read + Seek),
    limit: u64,
    parts: &mut Parts,
) -> Result<(), FormatError> {
    let length = source
        .seek(SeekFrom::End(0))
        .map_err(|error| FormatError::io("seek PNG end", error))?;
    // Base64 of a card at the metadata limit.
    let text_limit = limit.div_ceil(3).saturating_mul(4);
    let (mut ccv3, mut chara) = (None, None);
    let mut position = PNG_SIGNATURE_BYTES;
    // As the importer reads it: chunk by chunk to IEND, a later chunk of the same keyword winning.
    while position.saturating_add(8) <= length {
        source
            .seek(SeekFrom::Start(position))
            .map_err(|error| FormatError::io("seek PNG chunk", error))?;
        let mut header = [0_u8; 8];
        source
            .read_exact(&mut header)
            .map_err(|error| FormatError::io("read PNG chunk header", error))?;
        let data_length = u64::from(u32::from_be_bytes([header[0], header[1], header[2], header[3]]));
        let data_start = position + 8;
        if &header[4..] == b"IEND" || data_start + data_length > length {
            break;
        }
        if &header[4..] == b"tEXt" {
            let mut keyword = Vec::new();
            source
                .by_ref()
                .take(data_length.min(TEXT_KEYWORD_SCAN_BYTES))
                .read_to_end(&mut keyword)
                .map_err(|error| FormatError::io("read PNG text keyword", error))?;
            let slot = match keyword.iter().position(|byte| *byte == 0).map(|end| (&keyword[..end], end)) {
                Some((b"ccv3", end)) => Some((&mut ccv3, end)),
                Some((b"chara", end)) => Some((&mut chara, end)),
                _ => None,
            };
            if let Some((slot, end)) = slot {
                let value_length = data_length - end as u64 - 1;
                if value_length > text_limit {
                    return Err(FormatError::limit("PNG card text exceeds the metadata limit"));
                }
                source
                    .seek(SeekFrom::Start(data_start + end as u64 + 1))
                    .map_err(|error| FormatError::io("seek PNG card text", error))?;
                let mut value = vec![0_u8; value_length as usize];
                source
                    .read_exact(&mut value)
                    .map_err(|error| FormatError::io("read PNG card text", error))?;
                *slot = Some(value);
            }
        }
        position = data_start + data_length + 4;
    }
    if let Some(value) = ccv3 {
        parts.push(Part::Ccv3, &value)?;
    }
    if let Some(value) = chara {
        parts.push(Part::Chara, &value)?;
    }
    Ok(())
}

fn read_charx(
    source: &mut (impl Read + Seek),
    limit: u64,
    parts: &mut Parts,
) -> Result<(), FormatError> {
    // The footer gives where the archive starts, past a JPEG it is appended to.
    let Some(preflight) = preflight_zip(source, MAX_ZIP_ENTRIES, MAX_ZIP_DIRECTORY_BYTES, &|| false)? else {
        return Err(FormatError::invalid("update base file is not a ZIP archive"));
    };
    let view = ArchiveView::new(source, preflight.archive_start, preflight.archive_end)?;
    let mut archive = zip::ZipArchive::new(view)
        .map_err(|error| FormatError::invalid(format!("update base archive: {error}")))?;
    for (name, part) in [("card.json", Part::Card), ("module.risum", Part::Module)] {
        let mut entry = match archive.by_name(name) {
            Ok(entry) => entry,
            Err(zip::result::ZipError::FileNotFound) => continue,
            Err(error) => return Err(FormatError::invalid(format!("update base {name}: {error}"))),
        };
        // The importer refuses a card or module larger than this.
        if entry.size() > limit {
            return Err(FormatError::limit(format!("update base {name} exceeds the metadata limit")));
        }
        let bytes = match part {
            Part::Module => read_risum_metadata(&mut entry, limit)?,
            _ => read_bounded(&mut entry, limit, name)?,
        };
        parts.push(part, &bytes)?;
    }
    Ok(())
}

/// The metadata of a RISUM file, read up to the first asset frame.
fn read_risum_metadata(reader: &mut impl Read, limit: u64) -> Result<Vec<u8>, FormatError> {
    let mut header = [0_u8; 6];
    reader
        .read_exact(&mut header)
        .map_err(|_| FormatError::invalid("Risu module header is truncated"))?;
    if header[0] != 111 || header[1] != 0 {
        return Err(FormatError::invalid("not a Risu module"));
    }
    let length = u64::from(u32::from_le_bytes([header[2], header[3], header[4], header[5]]));
    if length > limit {
        return Err(FormatError::limit("Risu module metadata exceeds the metadata limit"));
    }
    let mut encoded = Vec::new();
    reader
        .take(length)
        .read_to_end(&mut encoded)
        .map_err(|error| FormatError::io("read Risu module metadata", error))?;
    if encoded.len() as u64 != length {
        return Err(FormatError::invalid("Risu module metadata is truncated"));
    }
    decode_rpack(&encoded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import_export_jobs::FormatErrorKind;
    use std::io::{Cursor, Write};

    const RPACK_MAP: &[u8; 512] = include_bytes!("../../../src/ts/rpack/rpack_map.bin");

    fn parts(reply: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        let mut rest = reply;
        while !rest.is_empty() {
            let length = u32::from_le_bytes(rest[1..5].try_into().unwrap()) as usize;
            out.push((rest[0], rest[5..5 + length].to_vec()));
            rest = &rest[5 + length..];
        }
        out
    }

    fn read(bytes: Vec<u8>, format: UpdateBaseFormat, limit: u64) -> Result<Vec<(u8, Vec<u8>)>, FormatError> {
        read_update_base_within(&mut Cursor::new(bytes), format, limit).map(|reply| parts(&reply))
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut chunk = (data.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(data);
        // The reader does not check CRCs, as the importer does not.
        chunk.extend_from_slice(&[0; 4]);
        chunk
    }

    fn risum(metadata: &[u8], tail: &[u8]) -> Vec<u8> {
        let mut out = vec![111, 0];
        out.extend_from_slice(&(metadata.len() as u32).to_le_bytes());
        out.extend(metadata.iter().map(|byte| RPACK_MAP[*byte as usize]));
        out.extend_from_slice(tail);
        out
    }

    #[test]
    fn reads_the_card_text_of_a_png_past_asset_chunks_and_a_truncated_last_one() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(png_chunk(b"IHDR", &[0; 13]));
        png.extend(png_chunk(b"tEXt", b"chara\0old"));
        png.extend(png_chunk(b"tEXt", b"chara-ext-asset_:0\0not read"));
        png.extend(png_chunk(b"tEXt", b"chara\0bmV3"));
        png.extend(png_chunk(b"IDAT", &[7; 32]));
        // An asset chunk that claims more bytes than the file has.
        png.extend_from_slice(&1_000_000_u32.to_be_bytes());
        png.extend_from_slice(b"tEXt");
        png.extend_from_slice(b"chara-ext-asset_:1\0");
        assert_eq!(read(png, UpdateBaseFormat::Png, 1024).unwrap(), vec![(Part::Chara as u8, b"bmV3".to_vec())]);
    }

    #[test]
    fn stops_reading_a_png_at_its_end_chunk_and_refuses_card_text_over_the_limit() {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(png_chunk(b"tEXt", b"ccv3\0djM="));
        png.extend(png_chunk(b"IEND", &[]));
        png.extend(png_chunk(b"tEXt", b"ccv3\0after"));
        // Base64 of a card at the limit is 4 bytes for every 3.
        assert_eq!(read(png, UpdateBaseFormat::Png, 3).unwrap(), vec![(Part::Ccv3 as u8, b"djM=".to_vec())]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(png_chunk(b"tEXt", b"ccv3\0djM9djM9"));
        let error = read(png, UpdateBaseFormat::Png, 3).unwrap_err();
        assert_eq!(error.kind, FormatErrorKind::LimitExceeded);
    }

    fn charx(prefix: &[u8], entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, data) in entries {
            writer.start_file(*name, zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored)).unwrap();
            writer.write_all(data).unwrap();
        }
        let archive = writer.finish().unwrap().into_inner();
        let mut out = prefix.to_vec();
        out.extend(archive);
        out
    }

    #[test]
    fn reads_the_card_and_module_of_a_charx_without_reading_its_assets() {
        let card = br#"{"spec":"chara_card_v3","data":{"name":"Luna"}}"#;
        let module = br#"{"type":"risuModule","module":{"name":"Luna"}}"#;
        // Asset frames the module has after its metadata, which are not read.
        let module_file = risum(module, &[1, 0xff, 0xff, 0xff, 0x7f]);
        let mut file = charx(b"", &[("assets/face.png", b"ASSET-BYTES"), ("card.json", card), ("module.risum", &module_file)]);
        // The asset no longer matches its checksum, which reading it would report.
        let at = file.windows(11).position(|window| window == b"ASSET-BYTES").unwrap();
        file[at] ^= 0xff;
        let expected = vec![(Part::Card as u8, card.to_vec()), (Part::Module as u8, module.to_vec())];
        assert_eq!(read(file.clone(), UpdateBaseFormat::Charx, 1024).unwrap(), expected);
        // A CHARX appended to a JPEG.
        let mut jpeg = vec![0xff, 0xd8, 0xff, 0xe0];
        jpeg.extend_from_slice(&[0; 64]);
        jpeg.extend_from_slice(&[0xff, 0xd9]);
        jpeg.extend(file);
        assert_eq!(read(jpeg, UpdateBaseFormat::Charx, 1024).unwrap(), expected);
    }

    #[test]
    fn returns_no_card_for_a_charx_without_one_and_refuses_a_card_over_the_limit() {
        assert_eq!(read(charx(b"", &[("other.json", b"{}")]), UpdateBaseFormat::Charx, 1024).unwrap(), vec![]);
        let error = read(charx(b"", &[("card.json", &[b' '; 64])]), UpdateBaseFormat::Charx, 32).unwrap_err();
        assert_eq!(error.kind, FormatErrorKind::LimitExceeded);
        let error = read(b"not an archive".to_vec(), UpdateBaseFormat::Charx, 1024).unwrap_err();
        assert_eq!(error.kind, FormatErrorKind::InvalidFormat);
    }

    #[test]
    fn reads_the_metadata_of_a_risum_up_to_its_first_asset_frame() {
        let module = br#"{"type":"risuModule","module":{"name":"M"}}"#;
        assert_eq!(read(risum(module, &[9, 9, 9]), UpdateBaseFormat::Risum, 1024).unwrap(), vec![(Part::Module as u8, module.to_vec())]);
        let error = read(b"{\"type\":\"risuModule\"}".to_vec(), UpdateBaseFormat::Risum, 1024).unwrap_err();
        assert_eq!(error.kind, FormatErrorKind::InvalidFormat);
        let error = read(risum(module, &[]), UpdateBaseFormat::Risum, 8).unwrap_err();
        assert_eq!(error.kind, FormatErrorKind::LimitExceeded);
    }

    #[test]
    fn reads_a_whole_json_file_within_the_limit() {
        assert_eq!(read(b"{\"spec\":1}".to_vec(), UpdateBaseFormat::Json, 10).unwrap(), vec![(Part::File as u8, b"{\"spec\":1}".to_vec())]);
        assert_eq!(read(b"{\"spec\":12}".to_vec(), UpdateBaseFormat::Json, 10).unwrap_err().kind, FormatErrorKind::LimitExceeded);
    }
}
