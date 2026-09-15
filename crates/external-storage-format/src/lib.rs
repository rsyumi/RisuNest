//! Bounded, provider/OS/PDS-independent byte formats shared by native and WASM.
pub mod catalog;
pub mod content_identity;
pub mod control;
pub mod crypto;
pub mod format;
pub mod logical_records;
pub mod pack;
pub mod section;
pub mod snapshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormatError(pub &'static str);
impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for FormatError {}
impl From<std::io::Error> for FormatError {
    fn from(_: std::io::Error) -> Self {
        Self("object-io-failed")
    }
}
pub type Result<T> = std::result::Result<T, FormatError>;
