//! Full-only binary profile: RNSF, u32 count, then codec=0, SHA256[32],
//! u64 big-endian length, exact bytes. Whole batch <= 8 MiB including framing.
use crate::{hash, validate_hash, Result, WireError};
pub const MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_BATCH_OBJECTS: usize = 1024;
pub struct FullFrame<'a> {
    pub hash: String,
    pub bytes: &'a [u8],
}
pub fn encode(objects: &[&[u8]]) -> Result<Vec<u8>> {
    if objects.len() > MAX_BATCH_OBJECTS {
        return Err(WireError("too-many-frames"));
    }
    let mut out = b"RNSF".to_vec();
    out.extend((objects.len() as u32).to_be_bytes());
    for bytes in objects {
        if bytes.len() > MAX_BATCH_BYTES.saturating_sub(out.len() + 41) {
            return Err(WireError("batch-too-large"));
        }
        out.push(0);
        let hash = hash(bytes);
        for i in (0..64).step_by(2) {
            out.push(u8::from_str_radix(&hash[i..i + 2], 16).unwrap());
        }
        out.extend((bytes.len() as u64).to_be_bytes());
        out.extend(*bytes);
    }
    Ok(out)
}
pub fn decode(bytes: &[u8]) -> Result<Vec<FullFrame<'_>>> {
    if bytes.len() > MAX_BATCH_BYTES {
        return Err(WireError("batch-too-large"));
    }
    let mut input = bytes;
    if take(&mut input, 4)? != b"RNSF" {
        return Err(WireError("invalid-magic"));
    }
    let count = u32::from_be_bytes(take(&mut input, 4)?.try_into().unwrap()) as usize;
    if count > MAX_BATCH_OBJECTS {
        return Err(WireError("too-many-frames"));
    }
    let mut frames = Vec::new();
    for _ in 0..count {
        if take(&mut input, 1)? != [0] {
            return Err(WireError("unsupported-codec"));
        }
        let digest: String = take(&mut input, 32)?
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        validate_hash(&digest)?;
        let length = u64::from_be_bytes(take(&mut input, 8)?.try_into().unwrap());
        if length > MAX_BATCH_BYTES as u64 {
            return Err(WireError("frame-too-large"));
        }
        let body = take(&mut input, length as usize)?;
        if hash(body) != digest {
            return Err(WireError("hash-mismatch"));
        }
        frames.push(FullFrame {
            hash: digest,
            bytes: body,
        });
    }
    if !input.is_empty() {
        return Err(WireError("trailing-bytes"));
    }
    Ok(frames)
}
fn take<'a>(input: &mut &'a [u8], count: usize) -> Result<&'a [u8]> {
    if count > input.len() {
        return Err(WireError("truncated-frame"));
    }
    let (head, tail) = input.split_at(count);
    *input = tail;
    Ok(head)
}
