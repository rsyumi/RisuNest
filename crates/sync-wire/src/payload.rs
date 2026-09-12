//! Application-independent byte segmentation. Index boundaries depend on content,
//! so an insertion does not renumber every following 128-message page.
use crate::{canonical, hash, validate_hash, Result, Sequence, WireError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub const MIN_CHUNK: usize = 256 * 1024;
pub const MAX_CHUNK: usize = 4 * 1024 * 1024;
pub const MAX_PARTS: usize = 1_000_000;
const MAX_DEPTH: usize = 8;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Part {
    pub hash: String,
    pub size: Sequence,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Index {
    Chunks { parts: Vec<Part> },
    Branches { parts: Vec<Part> },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Payload {
    pub content_hash: String,
    pub byte_length: Sequence,
    pub root: String,
}

fn checked_size(part: &Part) -> Result<u64> {
    validate_hash(&part.hash)?;
    part.size
        .as_str()
        .parse()
        .map_err(|_| WireError("payload-too-large"))
}
fn save_index(
    parts: Vec<Part>,
    branches: bool,
    save: &mut impl FnMut(&[u8]) -> Result<()>,
) -> Result<Part> {
    let mut size = 0u64;
    for part in &parts {
        size = size
            .checked_add(checked_size(part)?)
            .ok_or(WireError("payload-too-large"))?;
    }
    let bytes = canonical::encode(&if branches {
        Index::Branches { parts }
    } else {
        Index::Chunks { parts }
    })?;
    save(&bytes)?;
    Ok(Part {
        hash: hash(&bytes),
        size: size.into(),
    })
}
fn index_boundary(hash: &str) -> bool {
    u8::from_str_radix(&hash[..2], 16).is_ok_and(|b| b & 31 == 0)
}

pub fn build(reader: &mut impl Read, mut save: impl FnMut(&[u8]) -> Result<()>) -> Result<Payload> {
    let mut chunks = Vec::new();
    let mut bytes = Vec::with_capacity(MAX_CHUNK);
    let mut buffer = [0u8; 64 * 1024];
    let mut whole = Sha256::new();
    let mut total = 0u64;
    let mut rolling = 0u64;
    let mut window = [0u8; 64];
    let mut offset = 0usize;
    let factor = 257u64.wrapping_pow(63);
    loop {
        let n = reader
            .read(&mut buffer)
            .map_err(|_| WireError("payload-read-failed"))?;
        if n == 0 {
            break;
        }
        whole.update(&buffer[..n]);
        total = total
            .checked_add(n as u64)
            .ok_or(WireError("payload-too-large"))?;
        for &byte in &buffer[..n] {
            let old = window[offset];
            window[offset] = byte;
            offset = (offset + 1) % 64;
            rolling = rolling
                .wrapping_sub((old as u64).wrapping_mul(factor))
                .wrapping_mul(257)
                .wrapping_add(byte as u64);
            bytes.push(byte);
            if bytes.len() == MAX_CHUNK
                || (bytes.len() >= MIN_CHUNK && rolling & 0x1f_ffff == 0x12_3456)
            {
                save(&bytes)?;
                chunks.push(Part {
                    hash: hash(&bytes),
                    size: (bytes.len() as u64).into(),
                });
                bytes.clear();
                if chunks.len() > MAX_PARTS {
                    return Err(WireError("payload-too-large"));
                }
            }
        }
    }
    if !bytes.is_empty() || chunks.is_empty() {
        save(&bytes)?;
        chunks.push(Part {
            hash: hash(&bytes),
            size: (bytes.len() as u64).into(),
        });
    }
    let mut branches = false;
    let mut depth = 0;
    loop {
        depth += 1;
        if depth > MAX_DEPTH {
            return Err(WireError("payload-tree-too-deep"));
        }
        let mut next = Vec::new();
        let mut current = Vec::new();
        for part in chunks {
            let boundary = index_boundary(&part.hash);
            current.push(part);
            if current.len() == 64 || (current.len() >= 16 && boundary) {
                next.push(save_index(
                    std::mem::take(&mut current),
                    branches,
                    &mut save,
                )?);
            }
        }
        if !current.is_empty() {
            next.push(save_index(current, branches, &mut save)?);
        }
        if next.len() == 1 {
            return Ok(Payload {
                content_hash: format!("{:x}", whole.finalize()),
                byte_length: total.into(),
                root: next.remove(0).hash,
            });
        }
        chunks = next;
        branches = true;
    }
}

/// Validate the complete byte sequence before the caller activates staged data.
pub fn restore(
    payload: &Payload,
    mut load: impl FnMut(&str) -> Result<Vec<u8>>,
    output: &mut impl Write,
) -> Result<()> {
    validate_hash(&payload.root)?;
    validate_hash(&payload.content_hash)?;
    let expected: u64 = payload
        .byte_length
        .as_str()
        .parse()
        .map_err(|_| WireError("payload-too-large"))?;
    let mut pending = vec![(payload.root.clone(), expected, 0usize, false)];
    let mut whole = Sha256::new();
    let mut written = 0u64;
    let mut parts_seen = 0usize;
    while let Some((digest, size, depth, raw)) = pending.pop() {
        parts_seen += 1;
        if parts_seen > MAX_PARTS + MAX_PARTS / 15 || depth > MAX_DEPTH {
            return Err(WireError("payload-tree-too-deep"));
        }
        let bytes = load(&digest)?;
        if hash(&bytes) != digest {
            return Err(WireError("hash-mismatch"));
        }
        if raw {
            if bytes.len() > MAX_CHUNK || bytes.len() as u64 != size {
                return Err(WireError("target-size-mismatch"));
            }
            written = written
                .checked_add(size)
                .filter(|v| *v <= expected)
                .ok_or(WireError("target-size-mismatch"))?;
            whole.update(&bytes);
            output
                .write_all(&bytes)
                .map_err(|_| WireError("payload-write-failed"))?;
        } else {
            let index: Index = canonical::decode(&bytes, 64 * 1024)?;
            let (parts, raw) = match index {
                Index::Chunks { parts } => (parts, true),
                Index::Branches { parts } => (parts, false),
            };
            if parts.is_empty() || parts.len() > 64 {
                return Err(WireError("invalid-payload-index"));
            }
            let mut sum = 0u64;
            for part in &parts {
                sum = sum
                    .checked_add(checked_size(part)?)
                    .ok_or(WireError("payload-too-large"))?;
            }
            if sum != size {
                return Err(WireError("target-size-mismatch"));
            }
            for part in parts.into_iter().rev() {
                let size = checked_size(&part)?;
                pending.push((part.hash, size, depth + 1, raw));
            }
        }
    }
    if written != expected || format!("{:x}", whole.finalize()) != payload.content_hash {
        return Err(WireError("target-hash-mismatch"));
    }
    Ok(())
}
