use crate::{validate_hash, Result, WireError};
use serde::{Deserialize, Serialize};

/// Decimal strings keep remote sequences independent of JS numbers/local PDS revisions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Sequence(String);
impl TryFrom<String> for Sequence {
    type Error = WireError;
    fn try_from(value: String) -> Result<Self> {
        if value.is_empty()
            || value.len() > 64
            || !value.bytes().all(|b| b.is_ascii_digit())
            || (value.len() > 1 && value.starts_with('0'))
        {
            return Err(WireError("invalid-sequence"));
        }
        Ok(Self(value))
    }
}
impl From<Sequence> for String {
    fn from(value: Sequence) -> Self {
        value.0
    }
}
impl From<u64> for Sequence {
    fn from(value: u64) -> Self {
        Self(value.to_string())
    }
}
impl Sequence {
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub fn next(&self) -> Result<Self> {
        let mut bytes = self.0.as_bytes().to_vec();
        for digit in bytes.iter_mut().rev() {
            if *digit != b'9' {
                *digit += 1;
                return String::from_utf8(bytes).unwrap().try_into();
            }
            *digit = b'0';
        }
        bytes.insert(0, b'1');
        String::from_utf8(bytes).unwrap().try_into()
    }
}
impl Ord for Sequence {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}
impl PartialOrd for Sequence {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteHead {
    pub library_id: String,
    pub epoch: String,
    pub seq: Sequence,
    pub head_id: String,
    pub min_retained_seq: Sequence,
}
impl RemoteHead {
    pub fn same_revision(&self, other: &Self) -> bool {
        self.library_id == other.library_id
            && self.epoch == other.epoch
            && self.seq == other.seq
            && self.head_id == other.head_id
    }
    pub fn validate(&self) -> Result<()> {
        validate_id(&self.library_id)?;
        validate_id(&self.epoch)?;
        validate_hash(&self.head_id)?;
        if self.min_retained_seq > self.seq {
            return Err(WireError("invalid-head"));
        }
        Ok(())
    }
    pub fn etag(&self) -> String {
        format!("\"{}\"", self.head_id)
    }
}
pub fn validate_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(WireError("invalid-id"));
    }
    Ok(())
}
