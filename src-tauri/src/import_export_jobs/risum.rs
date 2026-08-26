use super::{staging::CreatedPayloads, FormatError, ImportLimits, JobStaging, StagedPayload};
use serde_json::Value;
use std::io::{self, Read};

const RPACK_MAP: &[u8; 512] = include_bytes!("../../../src/ts/rpack/rpack_map.bin");
const METADATA_READ_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RisumAsset {
    pub position: usize,
    pub declared_extension: String,
    pub payload: StagedPayload,
}

#[derive(Debug, PartialEq)]
pub struct ParsedRisum {
    pub metadata: Value,
    pub assets: Vec<RisumAsset>,
}

pub fn decode_rpack(encoded: &[u8]) -> Result<Vec<u8>, FormatError> {
    validate_rpack_map()?;
    Ok(encoded
        .iter()
        .map(|byte| RPACK_MAP[256 + *byte as usize])
        .collect())
}

pub fn parse_risum(
    reader: &mut impl Read,
    staging: &JobStaging,
    limits: &ImportLimits,
    cancelled: &impl Fn() -> bool,
) -> Result<ParsedRisum, FormatError> {
    check_cancel(cancelled)?;
    if read_byte(reader, "Risu module magic")? != 111 {
        return Err(FormatError::invalid("invalid Risu module magic"));
    }
    if read_byte(reader, "Risu module version")? != 0 {
        return Err(FormatError::invalid("unsupported Risu module version"));
    }

    let main_length = read_u32(reader, "Risu module metadata length")? as usize;
    if main_length > limits.max_metadata_bytes {
        return Err(FormatError::limit(
            "Risu module metadata exceeds its byte limit",
        ));
    }
    let encoded_main = read_exact_vec(reader, main_length, "Risu module metadata", cancelled)?;
    check_cancel(cancelled)?;
    let metadata: Value = serde_json::from_slice(&decode_rpack(&encoded_main)?)
        .map_err(|_| FormatError::invalid("Risu module metadata is not valid JSON"))?;
    let declared_extensions = module_asset_extensions(&metadata)?;
    if declared_extensions.len() > limits.max_payload_count {
        return Err(FormatError::limit(
            "Risu module asset count exceeds its limit",
        ));
    }

    let mut assets = Vec::with_capacity(declared_extensions.len());
    let mut total_payload_bytes = 0_u64;
    let mut created = CreatedPayloads::new(staging);
    loop {
        check_cancel(cancelled)?;
        let marker = read_byte(reader, "Risu module asset marker or terminator")?;
        if marker == 0 {
            let mut trailing = [0_u8; 1];
            match reader.read(&mut trailing) {
                Ok(0) => break,
                Ok(_) => return Err(FormatError::invalid("Risu module has trailing bytes")),
                Err(error) => {
                    return Err(FormatError::io("read after Risu module terminator", error))
                }
            }
        }
        if marker != 1 {
            return Err(FormatError::invalid("invalid Risu module asset marker"));
        }
        let position = assets.len();
        if position >= declared_extensions.len() {
            return Err(FormatError::invalid(
                "Risu module has more asset frames than metadata",
            ));
        }
        let encoded_length = read_u32(reader, "Risu module asset length")? as u64;
        if encoded_length > limits.max_payload_bytes {
            return Err(FormatError::limit(
                "Risu module asset exceeds its byte limit",
            ));
        }
        total_payload_bytes = total_payload_bytes
            .checked_add(encoded_length)
            .ok_or_else(|| FormatError::limit("Risu module aggregate asset length overflow"))?;
        if total_payload_bytes > limits.max_aggregate_payload_bytes {
            return Err(FormatError::limit(
                "Risu module assets exceed the aggregate byte limit",
            ));
        }

        let mut bounded = reader.take(encoded_length);
        let mut decoded = RpackReader::new(&mut bounded)?;
        let payload = staging.stage_reader(&mut decoded, encoded_length, cancelled)?;
        created.track(&payload);
        if payload.byte_size != encoded_length || bounded.limit() != 0 {
            return Err(FormatError::invalid("truncated Risu module asset frame"));
        }
        assets.push(RisumAsset {
            position,
            declared_extension: declared_extensions[position].clone(),
            payload,
        });
    }

    if assets.len() != declared_extensions.len() {
        return Err(FormatError::invalid(
            "Risu module asset frame count does not match metadata",
        ));
    }
    check_cancel(cancelled)?;
    created.commit();
    Ok(ParsedRisum { metadata, assets })
}

fn module_asset_extensions(metadata: &Value) -> Result<Vec<String>, FormatError> {
    if metadata.get("type").and_then(Value::as_str) != Some("risuModule") {
        return Err(FormatError::invalid("invalid Risu module metadata type"));
    }
    let module = metadata
        .get("module")
        .and_then(Value::as_object)
        .ok_or_else(|| FormatError::invalid("Risu module metadata is missing module"))?;
    let Some(assets) = module.get("assets") else {
        return Ok(Vec::new());
    };
    let assets = assets
        .as_array()
        .ok_or_else(|| FormatError::invalid("Risu module assets must be an array"))?;
    assets
        .iter()
        .enumerate()
        .map(|(position, asset)| {
            asset
                .as_array()
                .and_then(|tuple| tuple.get(2))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| {
                    FormatError::invalid(format!(
                        "Risu module asset {position} has no declared extension"
                    ))
                })
        })
        .collect()
}

struct RpackReader<R> {
    inner: R,
}

impl<R> RpackReader<R> {
    fn new(inner: R) -> Result<Self, FormatError> {
        validate_rpack_map()?;
        Ok(Self { inner })
    }
}

impl<R: Read> Read for RpackReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        for byte in &mut buffer[..read] {
            *byte = RPACK_MAP[256 + *byte as usize];
        }
        Ok(read)
    }
}

fn validate_rpack_map() -> Result<(), FormatError> {
    for decoded in 0..=u8::MAX {
        let encoded = RPACK_MAP[decoded as usize];
        if RPACK_MAP[256 + encoded as usize] != decoded {
            return Err(FormatError::invalid("bundled RPack map is inconsistent"));
        }
    }
    Ok(())
}

fn check_cancel(cancelled: &impl Fn() -> bool) -> Result<(), FormatError> {
    if cancelled() {
        Err(FormatError::cancelled())
    } else {
        Ok(())
    }
}

fn read_byte(reader: &mut impl Read, description: &str) -> Result<u8, FormatError> {
    let mut byte = [0_u8; 1];
    reader
        .read_exact(&mut byte)
        .map_err(|error| map_required_read(error, description))?;
    Ok(byte[0])
}

fn read_u32(reader: &mut impl Read, description: &str) -> Result<u32, FormatError> {
    let mut bytes = [0_u8; 4];
    reader
        .read_exact(&mut bytes)
        .map_err(|error| map_required_read(error, description))?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_exact_vec(
    reader: &mut impl Read,
    length: usize,
    description: &str,
    cancelled: &impl Fn() -> bool,
) -> Result<Vec<u8>, FormatError> {
    let mut bytes = vec![0_u8; length];
    let mut offset = 0;
    while offset < bytes.len() {
        check_cancel(cancelled)?;
        let end = offset
            .checked_add(METADATA_READ_BYTES)
            .unwrap_or(bytes.len())
            .min(bytes.len());
        let read = reader
            .read(&mut bytes[offset..end])
            .map_err(|error| map_required_read(error, description))?;
        if read == 0 {
            return Err(FormatError::invalid(format!("truncated {description}")));
        }
        offset += read;
    }
    Ok(bytes)
}

fn map_required_read(error: io::Error, description: &str) -> FormatError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        FormatError::invalid(format!("truncated {description}"))
    } else {
        FormatError::io(&format!("read {description}"), error)
    }
}
