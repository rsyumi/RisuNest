use super::{FormatError, Result};
use sha2::{Digest, Sha256};
use std::io::Read;

pub fn hash(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
pub fn hash_reader(reader: &mut impl Read, expected_length: u64) -> Result<[u8; 32]> {
    let mut buffer = [0u8; 64 * 1024];
    let mut digest = Sha256::new();
    let mut length = 0u64;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        length = length
            .checked_add(count as u64)
            .ok_or(FormatError("length-overflow"))?;
        if length > expected_length {
            return Err(FormatError("object-length-mismatch"));
        }
        digest.update(&buffer[..count]);
    }
    if length != expected_length {
        return Err(FormatError("object-length-mismatch"));
    }
    Ok(digest.finalize().into())
}
