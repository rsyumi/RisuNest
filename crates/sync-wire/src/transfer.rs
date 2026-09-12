use crate::{
    batch::{MAX_BATCH_BYTES, MAX_BATCH_OBJECTS},
    delta::Recipe,
    hash, validate_hash, Result, WireError,
};
/// A full chunk takes about 8.4 seconds at 1 Mbps, leaving room for RTT and
/// verification within request deadlines. The hard wire ceiling stays 8 MiB.
pub const UPLOAD_CHUNK_BYTES: usize = 1024 * 1024;
pub const PREFERRED_BATCH_BYTES: usize = 1024 * 1024;
#[derive(Debug)]
pub enum Frame {
    Full(Vec<u8>),
    Delta(Recipe),
    FullRequired { hash: String, size: u64 },
}
pub fn encode(frames: &[Frame]) -> Result<Vec<u8>> {
    if frames.len() > MAX_BATCH_OBJECTS {
        return Err(WireError("too-many-frames"));
    }
    let mut out = b"RNSB".to_vec();
    out.extend((frames.len() as u32).to_be_bytes());
    for frame in frames {
        let mut payload = Vec::new();
        match frame {
            Frame::Full(bytes) => {
                payload.push(0);
                put_hash(&mut payload, &hash(bytes));
                payload.extend((bytes.len() as u64).to_be_bytes());
                payload.extend(bytes);
            }
            Frame::Delta(recipe) => {
                payload.push(1);
                payload.extend(recipe.encode()?);
            }
            Frame::FullRequired { hash, size } => {
                validate_hash(hash)?;
                payload.push(2);
                put_hash(&mut payload, hash);
                payload.extend(size.to_be_bytes());
            }
        }
        if out.len() + 4 + payload.len() > MAX_BATCH_BYTES {
            return Err(WireError("batch-too-large"));
        }
        out.extend((payload.len() as u32).to_be_bytes());
        out.extend(payload);
    }
    Ok(out)
}
pub fn decode(bytes: &[u8]) -> Result<Vec<Frame>> {
    if bytes.len() > MAX_BATCH_BYTES {
        return Err(WireError("batch-too-large"));
    }
    let mut input = bytes;
    if take(&mut input, 4)? != b"RNSB" {
        return Err(WireError("invalid-magic"));
    }
    let count = u32::from_be_bytes(take(&mut input, 4)?.try_into().unwrap()) as usize;
    if count > MAX_BATCH_OBJECTS {
        return Err(WireError("too-many-frames"));
    }
    let mut frames = Vec::new();
    let mut restored = 0u64;
    for _ in 0..count {
        let size = u32::from_be_bytes(take(&mut input, 4)?.try_into().unwrap()) as usize;
        let mut payload = take(&mut input, size)?;
        let frame = match take(&mut payload, 1)?[0] {
            codec @ (0 | 2) => {
                let digest = get_hash(&mut payload)?;
                let size = u64::from_be_bytes(take(&mut payload, 8)?.try_into().unwrap());
                if codec == 2 {
                    Frame::FullRequired { hash: digest, size }
                } else {
                    if size > MAX_BATCH_BYTES as u64 {
                        return Err(WireError("frame-too-large"));
                    }
                    let bytes = take(&mut payload, size as usize)?;
                    if hash(bytes) != digest {
                        return Err(WireError("hash-mismatch"));
                    }
                    Frame::Full(bytes.to_vec())
                }
            }
            1 => {
                let recipe = Recipe::decode(payload)?;
                payload = &[];
                Frame::Delta(recipe)
            }
            _ => return Err(WireError("unsupported-codec")),
        };
        if !payload.is_empty() {
            return Err(WireError("trailing-bytes"));
        }
        restored += match &frame {
            Frame::Full(bytes) => bytes.len() as u64,
            Frame::Delta(recipe) => recipe.target_size,
            Frame::FullRequired { .. } => 0,
        };
        if restored > 32 * 1024 * 1024 {
            return Err(WireError("batch-materialization-limit"));
        }
        frames.push(frame);
    }
    if !input.is_empty() {
        return Err(WireError("trailing-bytes"));
    }
    Ok(frames)
}
fn put_hash(out: &mut Vec<u8>, digest: &str) {
    for i in (0..64).step_by(2) {
        out.push(u8::from_str_radix(&digest[i..i + 2], 16).unwrap());
    }
}
fn get_hash(input: &mut &[u8]) -> Result<String> {
    Ok(take(input, 32)?
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn take<'a>(input: &mut &'a [u8], count: usize) -> Result<&'a [u8]> {
    if count > input.len() {
        return Err(WireError("truncated-frame"));
    }
    let (head, tail) = input.split_at(count);
    *input = tail;
    Ok(head)
}
