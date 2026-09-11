//! RNSD exact-byte COPY/INSERT profile, not VCDIFF. Ordered CAS bases are
//! independent sources; COPY never references output or a patch chain.
use crate::{hash, validate_hash, Result, WireError};
use std::collections::HashMap;

pub const MAX_BASES: usize = 4;
pub const MAX_TARGET_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_BASE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_PATCH_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_OPS: usize = 262_144;
const BLOCK: usize = 64;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Base {
    pub hash: String,
    pub size: u64,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Op {
    Copy { base: u8, offset: u64, length: u32 },
    Insert(Vec<u8>),
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Recipe {
    pub bases: Vec<Base>,
    pub target_hash: String,
    pub target_size: u64,
    pub ops: Vec<Op>,
}

impl Recipe {
    pub fn validate(&self) -> Result<()> {
        validate_hash(&self.target_hash)?;
        if self.bases.len() > MAX_BASES
            || self.target_size > MAX_TARGET_BYTES as u64
            || self.ops.len() > MAX_OPS
        {
            return Err(WireError("delta-limit"));
        }
        let mut sum = 0u64;
        let mut seen = std::collections::BTreeSet::new();
        for base in &self.bases {
            validate_hash(&base.hash)?;
            sum = sum.checked_add(base.size).ok_or(WireError("delta-limit"))?;
            if !seen.insert(&base.hash) {
                return Err(WireError("duplicate-base"));
            }
        }
        if sum > MAX_BASE_BYTES as u64 {
            return Err(WireError("delta-limit"));
        }
        let mut output = 0u64;
        let mut literal = 0usize;
        for op in &self.ops {
            let length = match op {
                Op::Insert(bytes) => {
                    literal = literal
                        .checked_add(bytes.len())
                        .ok_or(WireError("delta-limit"))?;
                    bytes.len() as u64
                }
                Op::Copy {
                    base,
                    offset,
                    length,
                } => {
                    let source = self
                        .bases
                        .get(*base as usize)
                        .ok_or(WireError("invalid-copy"))?;
                    if offset
                        .checked_add(*length as u64)
                        .is_none_or(|v| v > source.size)
                    {
                        return Err(WireError("invalid-copy"));
                    }
                    *length as u64
                }
            };
            if length == 0 {
                return Err(WireError("empty-delta-op"));
            }
            output = output.checked_add(length).ok_or(WireError("delta-limit"))?;
            if output > self.target_size || literal > MAX_PATCH_BYTES {
                return Err(WireError("delta-limit"));
            }
        }
        if output != self.target_size {
            return Err(WireError("target-size-mismatch"));
        }
        Ok(())
    }
    pub fn apply(&self, bases: &[&[u8]]) -> Result<Vec<u8>> {
        self.validate()?;
        if bases.len() != self.bases.len() {
            return Err(WireError("missing-base"));
        }
        for (bytes, base) in bases.iter().zip(&self.bases) {
            if bytes.len() as u64 != base.size || hash(bytes) != base.hash {
                return Err(WireError("base-hash-mismatch"));
            }
        }
        let mut output = Vec::with_capacity(self.target_size as usize);
        for op in &self.ops {
            match op {
                Op::Insert(bytes) => output.extend_from_slice(bytes),
                Op::Copy {
                    base,
                    offset,
                    length,
                } => output.extend_from_slice(
                    &bases[*base as usize][*offset as usize..*offset as usize + *length as usize],
                ),
            }
        }
        if hash(&output) != self.target_hash {
            return Err(WireError("target-hash-mismatch"));
        }
        Ok(output)
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut out = b"RNSD".to_vec();
        put_hash(&mut out, &self.target_hash);
        out.extend(self.target_size.to_be_bytes());
        out.push(self.bases.len() as u8);
        for base in &self.bases {
            put_hash(&mut out, &base.hash);
            out.extend(base.size.to_be_bytes());
        }
        out.extend((self.ops.len() as u32).to_be_bytes());
        for op in &self.ops {
            match op {
                Op::Insert(bytes) => {
                    out.push(0);
                    out.extend((bytes.len() as u32).to_be_bytes());
                    out.extend(bytes);
                }
                Op::Copy {
                    base,
                    offset,
                    length,
                } => {
                    out.extend([1, *base]);
                    out.extend(offset.to_be_bytes());
                    out.extend(length.to_be_bytes());
                }
            }
            if out.len() > MAX_PATCH_BYTES {
                return Err(WireError("delta-limit"));
            }
        }
        Ok(out)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_PATCH_BYTES {
            return Err(WireError("delta-limit"));
        }
        let mut input = bytes;
        if take(&mut input, 4)? != b"RNSD" {
            return Err(WireError("invalid-magic"));
        }
        let target_hash = get_hash(&mut input)?;
        let target_size = u64::from_be_bytes(take(&mut input, 8)?.try_into().unwrap());
        let count = take(&mut input, 1)?[0] as usize;
        if count > MAX_BASES {
            return Err(WireError("delta-limit"));
        }
        let mut bases = Vec::new();
        for _ in 0..count {
            bases.push(Base {
                hash: get_hash(&mut input)?,
                size: u64::from_be_bytes(take(&mut input, 8)?.try_into().unwrap()),
            });
        }
        let count = u32::from_be_bytes(take(&mut input, 4)?.try_into().unwrap()) as usize;
        if count > MAX_OPS {
            return Err(WireError("delta-limit"));
        }
        let mut ops = Vec::new();
        for _ in 0..count {
            ops.push(match take(&mut input, 1)?[0] {
                0 => {
                    let length =
                        u32::from_be_bytes(take(&mut input, 4)?.try_into().unwrap()) as usize;
                    Op::Insert(take(&mut input, length)?.to_vec())
                }
                1 => Op::Copy {
                    base: take(&mut input, 1)?[0],
                    offset: u64::from_be_bytes(take(&mut input, 8)?.try_into().unwrap()),
                    length: u32::from_be_bytes(take(&mut input, 4)?.try_into().unwrap()),
                },
                _ => return Err(WireError("unknown-delta-op")),
            });
        }
        if !input.is_empty() {
            return Err(WireError("trailing-bytes"));
        }
        let recipe = Self {
            bases,
            target_hash,
            target_size,
            ops,
        };
        recipe.validate()?;
        Ok(recipe)
    }
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
fn fingerprint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0u64, |h, b| h.wrapping_mul(257).wrapping_add(*b as u64 + 1))
}

/// Bounded block index with rolling target search. Verify every fingerprint match
/// against exact bytes. Two candidates per source bound repeated-content work
/// without allowing an earlier source to hide matches in a later source.
pub fn create(bases: &[&[u8]], target: &[u8]) -> Result<Recipe> {
    if bases.len() > MAX_BASES
        || target.len() > MAX_TARGET_BYTES
        || bases.iter().map(|b| b.len()).sum::<usize>() > MAX_BASE_BYTES
    {
        return Err(WireError("delta-limit"));
    }
    let mut index = HashMap::<u64, Vec<(u8, usize)>>::new();
    for (id, base) in bases.iter().enumerate() {
        for offset in (0..base.len().saturating_sub(BLOCK - 1)).step_by(BLOCK) {
            let bucket = index
                .entry(fingerprint(&base[offset..offset + BLOCK]))
                .or_default();
            if bucket
                .iter()
                .filter(|(source, _)| *source == id as u8)
                .count()
                < 2
            {
                bucket.push((id as u8, offset));
            }
        }
    }
    let factor = 257u64.wrapping_pow((BLOCK - 1) as u32);
    let mut position = 0;
    let mut literal = 0;
    let mut ops = Vec::new();
    let mut rolling = None;
    while position + BLOCK <= target.len() {
        let key = *rolling.get_or_insert_with(|| fingerprint(&target[position..position + BLOCK]));
        let mut best = (0u8, 0usize, 0usize);
        if let Some(candidates) = index.get(&key) {
            for &(base, offset) in candidates {
                let source = bases[base as usize];
                if source[offset..offset + BLOCK] == target[position..position + BLOCK] {
                    let mut length = BLOCK;
                    while offset + length < source.len()
                        && position + length < target.len()
                        && source[offset + length] == target[position + length]
                    {
                        length += 1;
                    }
                    if length > best.2 {
                        best = (base, offset, length);
                    }
                }
            }
        }
        if best.2 >= BLOCK {
            if literal < position {
                ops.push(Op::Insert(target[literal..position].to_vec()));
            }
            ops.push(Op::Copy {
                base: best.0,
                offset: best.1 as u64,
                length: best.2 as u32,
            });
            position += best.2;
            literal = position;
            rolling = None;
        } else {
            rolling = target.get(position + BLOCK).map(|next| {
                key.wrapping_sub((target[position] as u64 + 1).wrapping_mul(factor))
                    .wrapping_mul(257)
                    .wrapping_add(*next as u64 + 1)
            });
            position += 1;
        }
        if ops.len() > MAX_OPS {
            return Err(WireError("delta-limit"));
        }
    }
    if literal < target.len() {
        ops.push(Op::Insert(target[literal..].to_vec()));
    }
    let recipe = Recipe {
        bases: bases
            .iter()
            .map(|b| Base {
                hash: hash(b),
                size: b.len() as u64,
            })
            .collect(),
        target_hash: hash(target),
        target_size: target.len() as u64,
        ops,
    };
    recipe.validate()?;
    Ok(recipe)
}
