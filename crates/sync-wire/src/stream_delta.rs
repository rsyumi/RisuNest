//! File-backed RNSL COPY/INSERT profile. No output references, patch chains, or
//! application semantics. The in-memory RNSD profile retains its smaller limits.
use crate::{
    delta::{Base, Op, Recipe, MAX_BASES, MAX_OPS, MAX_PATCH_BYTES},
    Result, WireError,
};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};

pub const MAX_FILE_BYTES: u64 = 1024 * 1024 * 1024 * 1024;
const BUFFER: usize = 64 * 1024;
const ANCHOR: usize = 64;
const MAX_ANCHORS: u64 = 262_144;

fn io<T>(value: std::io::Result<T>) -> Result<T> {
    value.map_err(|_| WireError("delta-io"))
}
pub fn validate(recipe: &Recipe) -> Result<()> {
    recipe.validate_limits(MAX_FILE_BYTES, MAX_FILE_BYTES)
}
pub fn encode(recipe: &Recipe) -> Result<Vec<u8>> {
    validate(recipe)?;
    recipe.encode_profile(b"RNSL")
}
pub fn decode(bytes: &[u8]) -> Result<Recipe> {
    let recipe = Recipe::decode_profile(bytes, b"RNSL")?;
    validate(&recipe)?;
    Ok(recipe)
}
fn fingerprint(bytes: &[u8]) -> u64 {
    bytes
        .iter()
        .fold(0u64, |h, b| h.wrapping_mul(257).wrapping_add(*b as u64 + 1))
}
/// Whole source verification is mandatory even when COPY uses only a small part.
pub fn verify<R: Read + Seek>(
    source: &mut R,
    expected: &Base,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<()> {
    io(source.seek(SeekFrom::Start(0)))?;
    let mut buffer = vec![0; BUFFER];
    let mut digest = Sha256::new();
    let mut size = 0u64;
    loop {
        check()?;
        let n = io(source.read(&mut buffer))?;
        if n == 0 {
            break;
        }
        size += n as u64;
        if size > expected.size {
            return Err(WireError("base-size-mismatch"));
        }
        digest.update(&buffer[..n]);
    }
    if size != expected.size || format!("{:x}", digest.finalize()) != expected.hash {
        return Err(WireError("base-hash-mismatch"));
    }
    Ok(())
}
/// Output is private until this function and the caller's durable CAS publish
/// finish. A failed hash or cancellation must never expose partial output.
pub fn apply<R: Read + Seek, W: Write>(
    recipe: &Recipe,
    sources: &mut [R],
    output: &mut W,
    mut check: impl FnMut() -> Result<()>,
) -> Result<()> {
    validate(recipe)?;
    if sources.len() != recipe.bases.len() {
        return Err(WireError("missing-base"));
    }
    for (source, base) in sources.iter_mut().zip(&recipe.bases) {
        verify(source, base, &mut check)?;
    }
    let mut buffer = vec![0; BUFFER];
    let mut digest = Sha256::new();
    for op in &recipe.ops {
        check()?;
        match op {
            Op::Insert(bytes) => {
                io(output.write_all(bytes))?;
                digest.update(bytes);
            }
            Op::Copy {
                base,
                offset,
                length,
            } => {
                let source = &mut sources[*base as usize];
                io(source.seek(SeekFrom::Start(*offset)))?;
                let mut left = *length as usize;
                while left != 0 {
                    check()?;
                    let n = left.min(BUFFER);
                    io(source.read_exact(&mut buffer[..n]))?;
                    io(output.write_all(&buffer[..n]))?;
                    digest.update(&buffer[..n]);
                    left -= n;
                }
            }
        }
    }
    if format!("{:x}", digest.finalize()) != recipe.target_hash {
        return Err(WireError("target-hash-mismatch"));
    }
    Ok(())
}
struct Window<'a, R> {
    source: &'a mut R,
    size: u64,
    start: u64,
    bytes: Vec<u8>,
}
impl<'a, R: Read + Seek> Window<'a, R> {
    fn new(source: &'a mut R, size: u64) -> Self {
        Self {
            source,
            size,
            start: u64::MAX,
            bytes: Vec::new(),
        }
    }
    fn at(&mut self, offset: u64, minimum: usize) -> Result<&[u8]> {
        if offset > self.size || minimum as u64 > self.size - offset {
            return Err(WireError("delta-io"));
        }
        if offset < self.start
            || offset + minimum as u64 > self.start.saturating_add(self.bytes.len() as u64)
        {
            self.start = offset;
            self.bytes
                .resize((self.size - offset).min(BUFFER as u64) as usize, 0);
            io(self.source.seek(SeekFrom::Start(offset)))?;
            io(self.source.read_exact(&mut self.bytes))?;
        }
        Ok(&self.bytes[(offset - self.start) as usize..])
    }
}
/// Sparse 64-byte anchors are indexed in a sorted, bounded vector. A 1 GiB base
/// uses a 4 KiB stride and 6 MiB index. Target search is rolling byte-by-byte so
/// insertions do not shift all subsequent block boundaries. Two candidates per
/// base cap repeated-content work. All matches are verified against exact bytes.
pub fn create<R: Read + Seek>(
    sources: &mut [R],
    identities: &[Base],
    target: &mut R,
    identity: Base,
    mut check: impl FnMut() -> Result<()>,
) -> Result<Recipe> {
    let total = identities
        .iter()
        .try_fold(0u64, |sum, b| sum.checked_add(b.size))
        .ok_or(WireError("delta-limit"))?;
    if sources.len() != identities.len()
        || identities.len() > MAX_BASES
        || total > MAX_FILE_BYTES
        || identity.size > MAX_FILE_BYTES
    {
        return Err(WireError("delta-limit"));
    }
    let stride = total.div_ceil(MAX_ANCHORS).max(4096).div_ceil(4096) * 4096;
    let mut index = Vec::<(u64, u8, u64)>::new();
    let mut buffer = vec![0; BUFFER];
    for (id, (source, base)) in sources.iter_mut().zip(identities).enumerate() {
        crate::validate_hash(&base.hash)?;
        io(source.seek(SeekFrom::Start(0)))?;
        let mut digest = Sha256::new();
        let mut position = 0;
        let mut next_anchor = 0;
        while position < base.size {
            check()?;
            let n = (base.size - position).min(BUFFER as u64) as usize;
            io(source.read_exact(&mut buffer[..n]))?;
            digest.update(&buffer[..n]);
            while next_anchor + ANCHOR as u64 <= position + n as u64 {
                let start = (next_anchor - position) as usize;
                index.push((
                    fingerprint(&buffer[start..start + ANCHOR]),
                    id as u8,
                    next_anchor,
                ));
                next_anchor += stride;
            }
            position += n as u64;
        }
        if io(source.read(&mut buffer[..1]))? != 0
            || format!("{:x}", digest.finalize()) != base.hash
        {
            return Err(WireError("base-hash-mismatch"));
        }
    }
    verify(target, &identity, &mut check)?;
    index.sort_unstable();
    let mut last = None;
    let mut count = 0;
    index.retain(|entry| {
        let key = (entry.0, entry.1);
        if last != Some(key) {
            last = Some(key);
            count = 0;
        }
        count += 1;
        count <= 2
    });
    let mut sources: Vec<_> = sources
        .iter_mut()
        .zip(identities)
        .map(|(s, b)| Window::new(s, b.size))
        .collect();
    let mut target = Window::new(target, identity.size);
    let mut position = 0;
    let mut ops = Vec::new();
    let mut literal = Vec::new();
    let mut literal_total = 0;
    let mut rolling = None;
    let factor = 257u64.wrapping_pow((ANCHOR - 1) as u32);
    while position + ANCHOR as u64 <= identity.size {
        if position % BUFFER as u64 == 0 {
            check()?;
        }
        let key = match rolling {
            Some(key) => key,
            None => fingerprint(&target.at(position, ANCHOR)?[..ANCHOR]),
        };
        let mut best = (0u8, 0u64, 0u64);
        let mut candidates = [0; MAX_BASES];
        let first = index.partition_point(|entry| entry.0 < key);
        for &(_, base, offset) in index[first..].iter().take_while(|entry| entry.0 == key) {
            if candidates[base as usize] >= 2 {
                continue;
            }
            candidates[base as usize] += 1;
            let source = &mut sources[base as usize];
            let limit = (source.size - offset).min(identity.size - position);
            let mut length = 0;
            while length < limit {
                check()?;
                let a = source.at(offset + length, 1)?;
                let b = target.at(position + length, 1)?;
                let n = a.len().min(b.len()).min((limit - length) as usize);
                let equal = a[..n]
                    .iter()
                    .zip(&b[..n])
                    .position(|(a, b)| a != b)
                    .unwrap_or(n);
                length += equal as u64;
                if equal != n {
                    break;
                }
            }
            if length > best.2 {
                best = (base, offset, length);
            }
        }
        if best.2 >= ANCHOR as u64 {
            if !literal.is_empty() {
                ops.push(Op::Insert(std::mem::take(&mut literal)));
            }
            let mut copied = 0;
            while copied < best.2 {
                let length = (best.2 - copied).min(u32::MAX as u64) as u32;
                ops.push(Op::Copy {
                    base: best.0,
                    offset: best.1 + copied,
                    length,
                });
                copied += length as u64;
            }
            position += best.2;
            rolling = None;
        } else {
            let first = target.at(position, 1)?[0];
            rolling = if position + (ANCHOR as u64) < identity.size {
                let next = target.at(position + ANCHOR as u64, 1)?[0];
                Some(
                    key.wrapping_sub((first as u64 + 1).wrapping_mul(factor))
                        .wrapping_mul(257)
                        .wrapping_add(next as u64 + 1),
                )
            } else {
                None
            };
            literal.push(first);
            literal_total += 1;
            position += 1;
        }
        if literal_total > MAX_PATCH_BYTES || ops.len() > MAX_OPS {
            return Err(WireError("delta-limit"));
        }
    }
    if position < identity.size {
        literal.extend_from_slice(target.at(position, (identity.size - position) as usize)?);
    }
    if !literal.is_empty() {
        ops.push(Op::Insert(literal));
    }
    let recipe = Recipe {
        bases: identities.to_vec(),
        target_hash: identity.hash,
        target_size: identity.size,
        ops,
    };
    encode(&recipe)?;
    Ok(recipe)
}
