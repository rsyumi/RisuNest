//! Deterministic bounded chunks. Pack targets never delay a short final chunk.
use super::{content_identity::hash, FormatError, Result};
use std::io::{Read, Write};
pub const MAX_CHUNK_BYTES: usize = 1024 * 1024;
pub const ENTRY_OVERHEAD: u64 = 8 + 32;

pub fn compress(bytes: &[u8]) -> Result<Vec<u8>> {
    if bytes.len() > MAX_CHUNK_BYTES {
        return Err(FormatError("chunk-limit-exceeded"));
    }
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

pub fn decompress(
    bytes: &[u8],
    expected_length: usize,
    expected_hash: &[u8; 32],
) -> Result<Vec<u8>> {
    if expected_length > MAX_CHUNK_BYTES || bytes.len() > MAX_CHUNK_BYTES + 1024 {
        return Err(FormatError("chunk-limit-exceeded"));
    }
    let mut decoder = flate2::read::ZlibDecoder::new(bytes);
    let mut output = Vec::new();
    decoder
        .by_ref()
        .take(expected_length as u64 + 1)
        .read_to_end(&mut output)?;
    if output.len() != expected_length
        || decoder.total_in() != bytes.len() as u64
        || hash(&output) != *expected_hash
    {
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
    if chunk.bytes.len() > MAX_CHUNK_BYTES || hash(&chunk.bytes) != chunk.hash {
        return Err(FormatError("invalid-chunk"));
    }
    writer.write_all(&(chunk.bytes.len() as u64).to_le_bytes())?;
    writer.write_all(&chunk.hash)?;
    writer.write_all(&chunk.bytes)?;
    Ok(ENTRY_OVERHEAD + chunk.bytes.len() as u64)
}
pub fn read_entry(reader: &mut impl Read, max_bytes: usize) -> Result<Chunk> {
    let mut length = [0; 8];
    reader.read_exact(&mut length)?;
    let length = u64::from_le_bytes(length);
    if length > max_bytes.min(MAX_CHUNK_BYTES) as u64 {
        return Err(FormatError("chunk-limit-exceeded"));
    }
    let mut expected = [0; 32];
    reader.read_exact(&mut expected)?;
    let mut bytes = vec![0; length as usize];
    reader.read_exact(&mut bytes)?;
    if hash(&bytes) != expected {
        return Err(FormatError("chunk-hash-mismatch"));
    }
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
}
