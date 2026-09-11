//! Format-independent sync protocol. Content bytes are never JSON-normalized.
pub mod batch;
pub mod canonical;
pub mod change_digest;
pub mod changes;
pub mod delta;
pub mod descriptor;
pub mod head;
pub mod payload;
pub mod stream_delta;
pub mod transfer;

pub use changes::*;
pub use head::*;
use sha2::{Digest, Sha256};

pub type Result<T> = std::result::Result<T, WireError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub &'static str);
impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for WireError {}

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn validate_hash(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(WireError("invalid-hash"));
    }
    Ok(())
}
