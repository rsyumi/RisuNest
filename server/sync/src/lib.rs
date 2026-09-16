pub mod config;
pub mod connection;
pub mod http;
pub mod management;
#[cfg(test)]
mod management_tests;
pub mod publication;
pub mod runtime;
pub mod store;
pub mod tunnel;
#[cfg(windows)]
mod tunnel_job;
pub mod workload;

pub const PROTOCOL_ID: &str = "risunest-sync/v1";
pub const STORE_FORMAT_ID: &str = "risunest-sync-store/v1";

#[derive(Debug)]
pub struct Error {
    pub code: &'static str,
    pub status: u16,
}
impl Error {
    pub fn new(code: &'static str, status: u16) -> Self {
        Self { code, status }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code)
    }
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::new("storage-io", 503)
    }
}
impl From<rusqlite::Error> for Error {
    fn from(_: rusqlite::Error) -> Self {
        Self::new("metadata-storage", 503)
    }
}
impl From<risunest_sync_wire::WireError> for Error {
    fn from(value: risunest_sync_wire::WireError) -> Self {
        let status = match value.0 {
            "metadata-too-large" | "batch-too-large" | "frame-too-large" => 413,
            _ => 400,
        };
        Self::new(value.0, status)
    }
}
pub type Result<T> = std::result::Result<T, Error>;

impl From<risunest_sync_connect::ConnectError> for Error {
    fn from(value: risunest_sync_connect::ConnectError) -> Self {
        Self::new(value.0, 400)
    }
}
