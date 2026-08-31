//! Shared verified byte reader for export writers that must load a pinned
//! payload fully into memory: streams with cancellation checks, guards the
//! declared size while reading, and fails closed when the bytes do not match
//! the leased hash. Writers that stream into a destination keep their own
//! loops because their sinks and failure codes differ deliberately.

use super::error::{cancelled, invalid_input, io_error};
use super::{JobControl, NativeJobError};
use sha2::{Digest, Sha256};
use std::io::Read;

pub(super) struct VerifiedReadLabels {
    pub(super) cancelled: &'static str,
    pub(super) capacity: &'static str,
    pub(super) size_changed: &'static str,
    pub(super) hash_changed: &'static str,
}

pub(super) fn read_verified_bytes(
    source: &mut dyn Read,
    expected_hash: &str,
    expected_size: u64,
    job: &JobControl,
    labels: &VerifiedReadLabels,
) -> Result<Vec<u8>, NativeJobError> {
    let capacity = usize::try_from(expected_size).map_err(|_| invalid_input(labels.capacity))?;
    let mut bytes = Vec::with_capacity(capacity);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        if job.is_cancel_requested() {
            return Err(cancelled(labels.cancelled));
        }
        let read = source.read(&mut buffer).map_err(io_error)?;
        if read == 0 {
            break;
        }
        if bytes.len().saturating_add(read) > capacity {
            return Err(invalid_input(labels.size_changed));
        }
        hasher.update(&buffer[..read]);
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.len() != capacity || hex::encode(hasher.finalize()) != expected_hash {
        return Err(invalid_input(labels.hash_changed));
    }
    Ok(bytes)
}
