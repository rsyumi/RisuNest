//! Portable ZIP64/SQLite archive primitives. File jobs own capture, publication and activation.
mod capture;
mod catalog;
mod internal;
mod inventory;
mod reader;
mod restore_inventory;
mod validation;
mod writer;

pub(crate) use capture::{capture_library, CapturedLibrary};
pub(crate) use catalog::Catalog;
pub(crate) use internal::create_verified_library_backup;
pub(crate) use reader::VerifiedArchive;
pub(crate) use restore_inventory::{PreservationReport, RestoreInventory};

use crate::local_backup::CancellationProbe;
use crate::persistent_store::StoreError;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Write};

const FORMAT: &str = "risunest-portable-backup";
const VERSION: u32 = 1;
const SMALL_DOCUMENT_LIMIT: u64 = 16 * 1024;
const PACK_BYTES: u64 = 256 * 1024 * 1024;
const BUFFER_BYTES: usize = 1024 * 1024;
const EMPTY_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Debug)]
pub(crate) enum Error {
    Io(io::Error),
    Sql(rusqlite::Error),
    Zip(zip::result::ZipError),
    Json(serde_json::Error),
    Store(StoreError),
    Invalid(&'static str),
    Cancelled,
    SourceNeedsPreservation,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => e.fmt(f),
            Self::Sql(e) => e.fmt(f),
            Self::Zip(e) => e.fmt(f),
            Self::Json(e) => e.fmt(f),
            Self::Store(e) => e.fmt(f),
            Self::Invalid(e) => f.write_str(e),
            Self::Cancelled => f.write_str("portable backup cancelled"),
            Self::SourceNeedsPreservation => {
                f.write_str("source requires a fresh preservation capture")
            }
        }
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sql(e)
    }
}
impl From<zip::result::ZipError> for Error {
    fn from(e: zip::result::ZipError) -> Self {
        Self::Zip(e)
    }
}
impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}
impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        Self::Store(e)
    }
}
type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Format {
    magic: String,
    format_version: u32,
    sqlite_schema_version: u32,
    source_app_build: String,
    capture_id: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Manifest {
    capture_id: String,
    catalog_bytes: String,
    pub(crate) catalog_sha256: String,
    pack_count: String,
    object_count: String,
    file_count: String,
    /// Container integrity is separate from application activation eligibility.
    pub(crate) repair_required: bool,
    pub(crate) library_included: bool,
    pub(crate) device_included: bool,
    pub(crate) profile: Profile,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Profile {
    Portable,
    SourceSqlite,
}

fn check(probe: &dyn CancellationProbe) -> Result<()> {
    if probe.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
fn decimal(value: &str) -> Result<u64> {
    if value.is_empty()
        || value.len() > 19
        || value.len() > 1 && value.starts_with('0')
        || !value.bytes().all(|v| v.is_ascii_digit())
    {
        return Err(Error::Invalid("noncanonical archive decimal"));
    }
    let parsed = value
        .parse::<u64>()
        .map_err(|_| Error::Invalid("archive decimal overflow"))?;
    if parsed > i64::MAX as u64 {
        return Err(Error::Invalid("archive decimal exceeds SQLite range"));
    }
    Ok(parsed)
}
fn sql_u64(value: i64) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Invalid("negative archive count or range"))
}
fn hash_valid(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|v| v.is_ascii_digit() || (b'a'..=b'f').contains(&v))
}

pub(crate) fn copy_hash(
    input: &mut (impl Read + ?Sized),
    output: &mut (impl Write + ?Sized),
    length: u64,
    probe: &dyn CancellationProbe,
) -> Result<String> {
    let mut hash = Sha256::new();
    let mut remaining = length;
    let mut buffer = vec![0; BUFFER_BYTES];
    while remaining > 0 {
        check(probe)?;
        let size = remaining.min(buffer.len() as u64) as usize;
        input.read_exact(&mut buffer[..size])?;
        output.write_all(&buffer[..size])?;
        hash.update(&buffer[..size]);
        remaining -= size as u64;
    }
    check(probe)?;
    Ok(hex::encode(hash.finalize()))
}

#[cfg(test)]
mod tests;
