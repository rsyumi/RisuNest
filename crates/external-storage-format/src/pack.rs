//! Deterministic bounded chunks. Pack targets never delay a short final chunk.
use super::{content_identity::hash, FormatError, Result};
use std::io::{Read, Write};
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;
pub const ENTRY_OVERHEAD: u64 = 8 + 8 + 32;
pub const COMPRESSION_LEVEL: i32 = 1;
pub const MAX_WINDOW_LOG: u32 = 20;
const RAW: u8 = 0;
const ZSTD: u8 = 1;

#[derive(Clone, Copy)]
pub enum CompressionPolicy {
    Text,
    AlreadyCompressed,
}

/// Reuse one encoder per CPU preparation worker, never one worker per asset.
pub struct ChunkEncoder(zstd::bulk::Compressor<'static>);
impl ChunkEncoder {
    pub fn new() -> Result<Self> {
        let mut encoder = zstd::bulk::Compressor::new(COMPRESSION_LEVEL)?;
        encoder.set_parameter(zstd::zstd_safe::CParameter::WindowLog(MAX_WINDOW_LOG))?;
        Ok(Self(encoder))
    }
    pub fn encode(&mut self, bytes: &[u8], policy: CompressionPolicy) -> Result<Vec<u8>> {
        if bytes.len() > MAX_CHUNK_BYTES {
            return Err(FormatError("chunk-limit-exceeded"));
        }
        if matches!(policy, CompressionPolicy::Text) {
            let compressed = self.0.compress(bytes)?;
            if compressed.len() < bytes.len() {
                let mut encoded = Vec::with_capacity(1 + compressed.len());
                encoded.push(ZSTD);
                encoded.extend_from_slice(&compressed);
                return Ok(encoded);
            }
        }
        let mut encoded = Vec::with_capacity(1 + bytes.len());
        encoded.push(RAW);
        encoded.extend_from_slice(bytes);
        Ok(encoded)
    }
}

pub fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    ChunkEncoder::new()?.encode(bytes, CompressionPolicy::Text)
}

pub fn decompress(
    bytes: &[u8],
    expected_length: usize,
    expected_hash: &[u8; 32],
) -> Result<Vec<u8>> {
    if expected_length > MAX_CHUNK_BYTES || bytes.len() > MAX_CHUNK_BYTES + 1 {
        return Err(FormatError("chunk-limit-exceeded"));
    }
    let Some((&codec, payload)) = bytes.split_first() else {
        return Err(FormatError("missing-chunk-codec"));
    };
    let output = match codec {
        RAW => {
            if payload.len() != expected_length {
                return Err(FormatError("chunk-length-mismatch"));
            }
            payload.to_vec()
        }
        ZSTD => {
            // Reject skippable frames, concatenated frames and bytes after the end.
            if !payload.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
                return Err(FormatError("invalid-zstd-frame"));
            }
            let mut decoder = zstd::stream::read::Decoder::with_buffer(payload)?.single_frame();
            decoder.window_log_max(MAX_WINDOW_LOG)?;
            let mut output = Vec::with_capacity(expected_length);
            decoder
                .by_ref()
                .take(expected_length as u64 + 1)
                .read_to_end(&mut output)?;
            if !decoder.finish().is_empty() {
                return Err(FormatError("trailing-compressed-bytes"));
            }
            output
        }
        _ => return Err(FormatError("unsupported-chunk-codec")),
    };
    if output.len() != expected_length || hash(&output) != *expected_hash {
        return Err(FormatError("compressed-chunk-integrity-failed"));
    }
    Ok(output)
}
pub struct Chunk {
    pub hash: [u8; 32],
    pub bytes: Vec<u8>,
}
pub fn chunks(
    reader: &mut impl Read,
    chunk_bytes: usize,
    mut consume: impl FnMut(Chunk) -> Result<()>,
) -> Result<()> {
    if chunk_bytes == 0 || chunk_bytes > MAX_CHUNK_BYTES {
        return Err(FormatError("invalid-chunk-limit"));
    }
    loop {
        let mut bytes = vec![0; chunk_bytes];
        let mut filled = 0;
        while filled < bytes.len() {
            let count = reader.read(&mut bytes[filled..])?;
            if count == 0 {
                break;
            }
            filled += count;
        }
        if filled == 0 {
            break;
        }
        bytes.truncate(filled);
        consume(Chunk {
            hash: hash(&bytes),
            bytes,
        })?;
    }
    Ok(())
}
pub fn write_entry(writer: &mut impl Write, chunk: &Chunk) -> Result<u64> {
    write_entry_with(
        writer,
        chunk,
        &mut ChunkEncoder::new()?,
        CompressionPolicy::Text,
    )
}
pub fn write_entry_with(
    writer: &mut impl Write,
    chunk: &Chunk,
    encoder: &mut ChunkEncoder,
    policy: CompressionPolicy,
) -> Result<u64> {
    if chunk.bytes.len() > MAX_CHUNK_BYTES || hash(&chunk.bytes) != chunk.hash {
        return Err(FormatError("invalid-chunk"));
    }
    writer.write_all(&(chunk.bytes.len() as u64).to_le_bytes())?;
    let encoded = encoder.encode(&chunk.bytes, policy)?;
    writer.write_all(&(encoded.len() as u64).to_le_bytes())?;
    writer.write_all(&chunk.hash)?;
    writer.write_all(&encoded)?;
    Ok(ENTRY_OVERHEAD + encoded.len() as u64)
}
pub fn read_entry(reader: &mut impl Read, max_bytes: usize) -> Result<Chunk> {
    let mut length = [0; 8];
    reader.read_exact(&mut length)?;
    let length = u64::from_le_bytes(length);
    if length > max_bytes.min(MAX_CHUNK_BYTES) as u64 {
        return Err(FormatError("chunk-limit-exceeded"));
    }
    let mut encoded_length = [0; 8];
    reader.read_exact(&mut encoded_length)?;
    let encoded_length = u64::from_le_bytes(encoded_length);
    if encoded_length == 0 || encoded_length > length + 1 {
        return Err(FormatError("chunk-limit-exceeded"));
    }
    let mut expected = [0; 32];
    reader.read_exact(&mut expected)?;
    let mut encoded = vec![0; encoded_length as usize];
    reader.read_exact(&mut encoded)?;
    let bytes = decompress(&encoded, length as usize, &expected)?;
    Ok(Chunk {
        hash: expected,
        bytes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn small_batches_close_without_padding_and_large_objects_stay_bounded() {
        let data = vec![42; MAX_CHUNK_BYTES + 20 * 1024];
        let mut sizes = Vec::new();
        chunks(&mut std::io::Cursor::new(&data), MAX_CHUNK_BYTES, |chunk| {
            sizes.push(chunk.bytes.len());
            let compressed = compress(&chunk.bytes)?;
            assert_eq!(
                decompress(&compressed, chunk.bytes.len(), &chunk.hash)?,
                chunk.bytes
            );
            let mut entry = Vec::new();
            write_entry(&mut entry, &chunk)?;
            assert_eq!(
                read_entry(&mut std::io::Cursor::new(entry), MAX_CHUNK_BYTES)?.bytes,
                chunk.bytes
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(sizes, [MAX_CHUNK_BYTES, 20 * 1024]);
    }
    #[test]
    fn decompression_checks_length_hash_and_trailing_bytes() {
        let bytes = vec![42; 4096];
        let mut compressed = compress(&bytes).unwrap();
        assert!(decompress(&compressed, 4095, &hash(&bytes)).is_err());
        assert!(decompress(&compressed, 4096, &[0; 32]).is_err());
        compressed.push(0);
        assert!(decompress(&compressed, 4096, &hash(&bytes)).is_err());
    }
    #[test]
    fn media_stays_raw_and_frames_cannot_expand_or_concatenate() {
        let data = vec![42; 4096];
        let raw = ChunkEncoder::new()
            .unwrap()
            .encode(&data, CompressionPolicy::AlreadyCompressed)
            .unwrap();
        assert_eq!(raw[0], RAW);
        assert_eq!(decompress(&raw, data.len(), &hash(&data)).unwrap(), data);
        assert_eq!(compress(&[]).unwrap(), [RAW]);
        let compressed = compress(&data).unwrap();
        assert_eq!(compressed[0], ZSTD);
        for end in [1, 5, compressed.len() - 1] {
            assert!(decompress(&compressed[..end], data.len(), &hash(&data)).is_err());
        }
        let mut doubled = compressed.clone();
        doubled.extend_from_slice(&compressed[1..]);
        assert!(decompress(&doubled, data.len(), &hash(&data)).is_err());
        // Streaming frame advertises an excessive window before any output.
        let mut huge_window = compressed.clone();
        huge_window[5] = 0; // no content size, non single segment
        huge_window[6] = 0xa0; // window log 30
        assert!(decompress(&huge_window, data.len(), &hash(&data)).is_err());
        let mut unknown = raw;
        unknown[0] = 2;
        assert!(decompress(&unknown, data.len(), &hash(&data)).is_err());
    }
}
