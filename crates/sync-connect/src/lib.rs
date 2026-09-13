//! Registration and opaque endpoint discovery shared by the daemon and native app.
mod directory;
pub mod media;
mod registration;

pub use directory::{
    generate_directory, open_endpoint, seal_endpoint, Directory, MAX_ENVELOPE_BYTES,
    MAX_ENVELOPE_TEXT,
};
pub use registration::{
    validate_endpoint, Registration, MAX_REGISTRATION_URI_BYTES, MAX_URL_BYTES, REGISTRATION_PREFIX,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectError(pub &'static str);
impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for ConnectError {}
pub type Result<T> = std::result::Result<T, ConnectError>;

pub(crate) fn decode(text: &str, max: usize) -> Result<Vec<u8>> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    if text.is_empty()
        || text.len() > max
        || !text
            .bytes()
            .all(|v| v.is_ascii_alphanumeric() || v == b'-' || v == b'_')
    {
        return Err(ConnectError("invalid-connection-encoding"));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(text)
        .map_err(|_| ConnectError("invalid-connection-encoding"))?;
    if URL_SAFE_NO_PAD.encode(&bytes) != text {
        return Err(ConnectError("invalid-connection-encoding"));
    }
    Ok(bytes)
}
