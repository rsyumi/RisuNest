use sha2::{Digest, Sha256};

const MAGIC: &[u8; 4] = b"ROMF";
const VERSION: u8 = 1;
const HEADER_BYTES: usize = 9;
const MIN_ENTRY_BYTES: usize = 13;

pub const OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES: usize = u32::MAX as usize;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnerManifestEntry {
    pub tuple: [String; 3],
    pub payload_hash: Option<[u8; 32]>,
}

// The property-level codec surface mirrors the TypeScript twin and is exercised
// by the golden parity integration test (tests/owner_manifest_codec.rs), which
// compiles this file via #[path], so the lib build cannot see that usage.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OwnerManifestProperty {
    Absent,
    Present(Vec<OwnerManifestEntry>),
}

#[derive(Debug)]
pub struct OwnerManifestCodecError(&'static str);

impl std::fmt::Display for OwnerManifestCodecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for OwnerManifestCodecError {}

struct ByteReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> ByteReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.bytes.len() - self.offset
    }

    fn read_u8(&mut self, context: &'static str) -> Result<u8, OwnerManifestCodecError> {
        Ok(self.read_bytes(1, context)?[0])
    }

    fn read_u32(&mut self, context: &'static str) -> Result<u32, OwnerManifestCodecError> {
        let bytes: [u8; 4] = self
            .read_bytes(4, context)?
            .try_into()
            .expect("four-byte slice");
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_bytes(
        &mut self,
        length: usize,
        context: &'static str,
    ) -> Result<&'a [u8], OwnerManifestCodecError> {
        if length > self.remaining() {
            return Err(OwnerManifestCodecError(context));
        }
        let end = self.offset + length;
        let result = &self.bytes[self.offset..end];
        self.offset = end;
        Ok(result)
    }
}

fn write_string(bytes: &mut Vec<u8>, value: &str) -> Result<(), OwnerManifestCodecError> {
    let length = u32::try_from(value.len())
        .map_err(|_| OwnerManifestCodecError("owner manifest string exceeds V1 size limit"))?;
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

fn read_string(reader: &mut ByteReader<'_>) -> Result<String, OwnerManifestCodecError> {
    let length = reader.read_u32("truncated string length")? as usize;
    let bytes = reader.read_bytes(length, "truncated string")?;
    let value = std::str::from_utf8(bytes)
        .map_err(|_| OwnerManifestCodecError("invalid UTF-8 in owner manifest string"))?;
    Ok(value.to_owned())
}

pub fn encode_owner_manifest(
    entries: &[OwnerManifestEntry],
) -> Result<Vec<u8>, OwnerManifestCodecError> {
    encode_owner_manifest_with_limit(entries, OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES)
}

pub fn encode_owner_manifest_with_limit(
    entries: &[OwnerManifestEntry],
    maximum_canonical_bytes: usize,
) -> Result<Vec<u8>, OwnerManifestCodecError> {
    let entry_count = u32::try_from(entries.len())
        .map_err(|_| OwnerManifestCodecError("owner manifest entry count exceeds V1 limit"))?;
    let maximum_canonical_bytes =
        maximum_canonical_bytes.min(OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES);
    let mut canonical_bytes = HEADER_BYTES;
    for entry in entries {
        for value in &entry.tuple {
            let string_bytes = u32::try_from(value.len()).map_err(|_| {
                OwnerManifestCodecError("owner manifest string exceeds V1 size limit")
            })?;
            canonical_bytes = canonical_bytes
                .checked_add(4)
                .and_then(|size| size.checked_add(string_bytes as usize))
                .ok_or(OwnerManifestCodecError(
                    "owner manifest exceeds V1 size limit",
                ))?;
        }
        canonical_bytes = canonical_bytes
            .checked_add(1 + usize::from(entry.payload_hash.is_some()) * 32)
            .ok_or(OwnerManifestCodecError(
                "owner manifest exceeds V1 size limit",
            ))?;
        if canonical_bytes > maximum_canonical_bytes {
            return Err(OwnerManifestCodecError(
                "owner manifest exceeds V1 size limit",
            ));
        }
    }
    if canonical_bytes > maximum_canonical_bytes {
        return Err(OwnerManifestCodecError(
            "owner manifest exceeds V1 size limit",
        ));
    }

    let mut bytes = Vec::with_capacity(canonical_bytes);
    bytes.extend_from_slice(MAGIC);
    bytes.push(VERSION);
    bytes.extend_from_slice(&entry_count.to_le_bytes());

    for entry in entries {
        for value in &entry.tuple {
            write_string(&mut bytes, value)?;
        }
        match entry.payload_hash {
            None => bytes.push(0),
            Some(hash) => {
                bytes.push(1);
                bytes.extend_from_slice(&hash);
            }
        }
    }
    Ok(bytes)
}

pub fn decode_owner_manifest(
    bytes: &[u8],
) -> Result<Vec<OwnerManifestEntry>, OwnerManifestCodecError> {
    decode_owner_manifest_with_limit(bytes, OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES)
}

pub fn decode_owner_manifest_with_limit(
    bytes: &[u8],
    maximum_canonical_bytes: usize,
) -> Result<Vec<OwnerManifestEntry>, OwnerManifestCodecError> {
    let maximum_canonical_bytes =
        maximum_canonical_bytes.min(OWNER_MANIFEST_V1_MAX_CANONICAL_BYTES);
    if bytes.len() > maximum_canonical_bytes {
        return Err(OwnerManifestCodecError(
            "owner manifest exceeds V1 size limit",
        ));
    }
    let mut reader = ByteReader::new(bytes);
    if reader.read_bytes(4, "truncated owner manifest magic")? != MAGIC {
        return Err(OwnerManifestCodecError("invalid owner manifest magic"));
    }
    if reader.read_u8("truncated owner manifest version")? != VERSION {
        return Err(OwnerManifestCodecError(
            "unsupported owner manifest version",
        ));
    }

    let entry_count = reader.read_u32("truncated owner manifest entry count")? as usize;
    if entry_count > reader.remaining() / MIN_ENTRY_BYTES {
        return Err(OwnerManifestCodecError(
            "owner manifest entry count exceeds remaining bytes",
        ));
    }
    let mut entries = Vec::with_capacity(entry_count);
    for _ in 0..entry_count {
        let tuple = [
            read_string(&mut reader)?,
            read_string(&mut reader)?,
            read_string(&mut reader)?,
        ];
        let payload_hash = match reader.read_u8("truncated payload hash marker")? {
            0 => None,
            1 => Some(
                reader
                    .read_bytes(32, "truncated payload hash")?
                    .try_into()
                    .expect("32-byte slice"),
            ),
            _ => return Err(OwnerManifestCodecError("invalid payload hash marker")),
        };
        entries.push(OwnerManifestEntry {
            tuple,
            payload_hash,
        });
    }
    if reader.remaining() != 0 {
        return Err(OwnerManifestCodecError(
            "trailing bytes after owner manifest",
        ));
    }
    Ok(entries)
}

// Mirrors the TypeScript twin; used by the golden parity integration test.
#[allow(dead_code)]
pub fn encode_owner_manifest_property(
    property: &OwnerManifestProperty,
) -> Result<Option<Vec<u8>>, OwnerManifestCodecError> {
    match property {
        OwnerManifestProperty::Absent => Ok(None),
        OwnerManifestProperty::Present(entries) => encode_owner_manifest(entries).map(Some),
    }
}

// Mirrors the TypeScript twin; used by the golden parity integration test.
#[allow(dead_code)]
pub fn decode_owner_manifest_property(
    present: bool,
    bytes: Option<&[u8]>,
) -> Result<OwnerManifestProperty, OwnerManifestCodecError> {
    match (present, bytes) {
        (false, None) => Ok(OwnerManifestProperty::Absent),
        (false, Some(_)) => Err(OwnerManifestCodecError(
            "absent property cannot have manifest bytes",
        )),
        (true, None) => Err(OwnerManifestCodecError(
            "present property requires manifest bytes",
        )),
        (true, Some(bytes)) => decode_owner_manifest(bytes).map(OwnerManifestProperty::Present),
    }
}

pub fn owner_manifest_identity(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}
