//! Shared framed-journal codec for the asset repository's durable journals
//! (CAS job pins and staged asset migrations): a u32 little-endian length,
//! the JSON payload, and a SHA-256 checksum per record, capped at 64 KiB.
//! Scanning stops at a truncated tail and reports where the valid prefix
//! ends; each journal decides its own recovery policy (job pins recover
//! under the repository mutation lock, migrations recover inline only when
//! their caller allows it).

use serde::de::DeserializeOwned;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};

pub(super) const MAX_JOURNAL_RECORD_BYTES: usize = 64 * 1024;

pub(super) struct JournalFrameLabels {
    pub(super) record_too_large: &'static str,
    pub(super) length_invalid: &'static str,
    pub(super) checksum_mismatch: &'static str,
}

pub(super) fn write_frame<T: Serialize>(
    file: &mut File,
    record: &T,
    sync: bool,
    labels: &JournalFrameLabels,
) -> io::Result<()> {
    let payload = serde_json::to_vec(record).map_err(json_error)?;
    if payload.len() > MAX_JOURNAL_RECORD_BYTES {
        return invalid_data(labels.record_too_large);
    }
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(ErrorKind::InvalidData, labels.record_too_large))?;
    file.write_all(&length.to_le_bytes())?;
    file.write_all(&payload)?;
    file.write_all(&Sha256::digest(&payload))?;
    if sync {
        file.flush()?;
        file.sync_data()?;
    }
    Ok(())
}

/// Byte length of the valid frame prefix when the file ends in a truncated
/// frame; `None` when every frame was complete.
pub(super) struct JournalScan {
    pub(super) incomplete_tail: Option<u64>,
}

pub(super) fn scan_frames<T: DeserializeOwned>(
    file: &mut File,
    labels: &JournalFrameLabels,
    mut visitor: impl FnMut(T) -> io::Result<()>,
) -> io::Result<JournalScan> {
    file.seek(SeekFrom::Start(0))?;
    let mut valid_length = 0_u64;
    loop {
        let mut length_bytes = [0_u8; 4];
        match read_exact_or_eof(file, &mut length_bytes)? {
            ExactRead::CleanEof => break,
            ExactRead::Partial => {
                return Ok(JournalScan {
                    incomplete_tail: Some(valid_length),
                });
            }
            ExactRead::Complete => {}
        }
        let length = u32::from_le_bytes(length_bytes) as usize;
        if length > MAX_JOURNAL_RECORD_BYTES {
            return invalid_data(labels.length_invalid);
        }
        let mut payload = vec![0; length];
        if read_exact_or_eof(file, &mut payload)? != ExactRead::Complete {
            return Ok(JournalScan {
                incomplete_tail: Some(valid_length),
            });
        }
        let mut checksum = [0_u8; 32];
        if read_exact_or_eof(file, &mut checksum)? != ExactRead::Complete {
            return Ok(JournalScan {
                incomplete_tail: Some(valid_length),
            });
        }
        if Sha256::digest(&payload).as_slice() != checksum {
            return invalid_data(labels.checksum_mismatch);
        }
        let record: T = serde_json::from_slice(&payload).map_err(json_error)?;
        visitor(record)?;
        valid_length = file.stream_position()?;
    }
    Ok(JournalScan {
        incomplete_tail: None,
    })
}

/// Truncates a journal to its valid prefix after a torn tail frame.
pub(super) fn truncate_to_valid_prefix(file: &mut File, valid_length: u64) -> io::Result<()> {
    file.set_len(valid_length)?;
    file.sync_data()?;
    file.seek(SeekFrom::Start(valid_length))?;
    Ok(())
}

#[derive(Eq, PartialEq)]
enum ExactRead {
    Complete,
    CleanEof,
    Partial,
}

fn read_exact_or_eof(reader: &mut impl Read, target: &mut [u8]) -> io::Result<ExactRead> {
    let mut offset = 0;
    while offset < target.len() {
        match reader.read(&mut target[offset..]) {
            Ok(0) if offset == 0 => return Ok(ExactRead::CleanEof),
            Ok(0) => return Ok(ExactRead::Partial),
            Ok(read) => offset += read,
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(ExactRead::Complete)
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, error)
}

fn invalid_data<T>(message: impl Into<String>) -> io::Result<T> {
    Err(io::Error::new(ErrorKind::InvalidData, message.into()))
}
