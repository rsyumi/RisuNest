//! Bytes of MessagePack `bin` values held beside the values that contain them.
//! A converted `bin` is an object with one member per byte, which costs about
//! a hundred and thirty bytes per byte as a `Value`. A placeholder object
//! stands in for it instead, and the writers here write the object the
//! conversion makes wherever a placeholder is.

use super::{lww, StoreResult};
use risunest_sync_wire::{payload_value, WireError};
use serde::ser::{Error as _, SerializeMap, SerializeSeq};
use serde::{Serialize, Serializer};
use serde_json::{Map, Value};
use std::io::Write;

/// The bytes behind the placeholders of a batch of values. The default holds
/// none, and no object is a placeholder for it.
#[derive(Default)]
pub(crate) struct HeldBinaries {
    /// The only key of a placeholder, whose value is the bytes' index. It is
    /// distinct for each restore, so a source object does not hold it.
    key: Option<String>,
    /// The index of `bytes[0]`. Released bytes keep their indexes.
    first: usize,
    bytes: Vec<Vec<u8>>,
}

impl HeldBinaries {
    pub(crate) fn new(nonce: &str) -> Self {
        Self { key: Some(format!("\u{0}risunest-binary:{nonce}")), first: 0, bytes: Vec::new() }
    }

    /// Whether an object key is the placeholder key, which a source must not hold.
    pub(crate) fn is_placeholder_key(&self, key: &str) -> bool {
        self.key.as_deref() == Some(key)
    }

    /// Holds `bytes` and returns the placeholder that stands for them.
    pub(crate) fn hold(&mut self, bytes: Vec<u8>) -> Value {
        let key = self.key.clone().expect("held binaries need a placeholder key");
        let index = self.next_index();
        self.bytes.push(bytes);
        Value::Object(Map::from_iter([(key, Value::from(index))]))
    }

    /// The index the next held bytes get.
    pub(crate) fn next_index(&self) -> usize {
        self.first + self.bytes.len()
    }

    /// Drops the bytes held from `index` on, for values that were not staged.
    pub(crate) fn truncate(&mut self, index: usize) {
        self.bytes.truncate(index.saturating_sub(self.first));
    }

    /// Drops the bytes held before `index`, once the values that hold them are written.
    pub(crate) fn release_before(&mut self, index: usize) {
        let count = index.saturating_sub(self.first).min(self.bytes.len());
        self.bytes.drain(..count);
        self.first += count;
    }

    /// The bytes a placeholder stands for, or `None` for any other object.
    fn binary(&self, object: &Map<String, Value>) -> Result<Option<&[u8]>, &'static str> {
        let Some(key) = &self.key else { return Ok(None) };
        if object.len() != 1 {
            return Ok(None);
        }
        let Some(index) = object.get(key) else { return Ok(None) };
        index
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| index.checked_sub(self.first))
            .and_then(|index| self.bytes.get(index))
            .map(|bytes| Some(bytes.as_slice()))
            .ok_or("a held binary was released before its value was written")
    }

    /// `value` with each placeholder replaced by the object it stands for.
    pub(crate) fn expand(&self, value: &Value) -> StoreResult<Value> {
        Ok(match value {
            Value::Object(object) => match self.binary(object).map_err(lww::error)? {
                Some(bytes) => Value::Object(
                    bytes.iter().enumerate().map(|(index, byte)| (index.to_string(), Value::from(*byte))).collect(),
                ),
                None => Value::Object(
                    object
                        .iter()
                        .map(|(key, value)| Ok((key.clone(), self.expand(value)?)))
                        .collect::<StoreResult<_>>()?,
                ),
            },
            Value::Array(values) => Value::Array(values.iter().map(|value| self.expand(value)).collect::<StoreResult<_>>()?),
            value => value.clone(),
        })
    }

    /// Serializes as the expanded value would.
    pub(crate) fn expanded<'a>(&'a self, value: &'a Value) -> Expanded<'a> {
        Expanded { value, binaries: self }
    }

    /// What `serde_json::to_string` writes for the expanded value.
    pub(crate) fn json(&self, value: &Value) -> StoreResult<String> {
        Ok(serde_json::to_string(&self.expanded(value))?)
    }

    /// What `payload_value::encode` writes for the expanded value.
    pub(crate) fn canonical(&self, value: &Value) -> StoreResult<Vec<u8>> {
        if self.key.is_none() {
            return payload_value::encode(value).map_err(lww::error);
        }
        let mut out = Vec::new();
        self.write_canonical(value, &mut out, 0)?;
        Ok(out)
    }

    // The same steps as `payload_value::encode`. A placeholder is an object at
    // the depth of the object it stands for, so the depth limit applies alike.
    fn write_canonical(&self, value: &Value, out: &mut Vec<u8>, depth: usize) -> StoreResult<()> {
        if matches!(value, Value::Object(_) | Value::Array(_)) && depth >= payload_value::DEPTH_LIMIT {
            return Err(invalid_payload());
        }
        match value {
            Value::Object(object) => {
                out.push(b'{');
                if let Some(bytes) = self.binary(object).map_err(lww::error)? {
                    // Index keys are already in the order an encoding sorts them into.
                    for (index, byte) in bytes.iter().enumerate() {
                        if index > 0 {
                            out.push(b',');
                        }
                        write!(out, "\"{index}\":").map_err(|_| invalid_payload())?;
                        payload_value::write_number(f64::from(*byte), out).map_err(lww::error)?;
                    }
                } else {
                    let mut keys: Vec<_> = object.keys().collect();
                    keys.sort_by(|a, b| payload_value::key_order(a, b));
                    for (index, key) in keys.into_iter().enumerate() {
                        if index > 0 {
                            out.push(b',');
                        }
                        serde_json::to_writer(&mut *out, key).map_err(|_| invalid_payload())?;
                        out.push(b':');
                        self.write_canonical(&object[key], out, depth + 1)?;
                    }
                }
                out.push(b'}');
            }
            Value::Array(values) => {
                out.push(b'[');
                for (index, value) in values.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    self.write_canonical(value, out, depth + 1)?;
                }
                out.push(b']');
            }
            Value::Number(number) => {
                let number = number.as_f64().ok_or_else(|| lww::error(WireError("unsupported-payload-number")))?;
                payload_value::write_number(number, out).map_err(lww::error)?;
            }
            _ => serde_json::to_writer(&mut *out, value).map_err(|_| invalid_payload())?,
        }
        Ok(())
    }
}

fn invalid_payload() -> super::StoreError {
    lww::error(WireError("invalid-payload-json"))
}

/// A value whose placeholders serialize as the objects they stand for.
pub(crate) struct Expanded<'a> {
    value: &'a Value,
    binaries: &'a HeldBinaries,
}

impl Serialize for Expanded<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.binaries.key.is_none() {
            return self.value.serialize(serializer);
        }
        // The same calls `Value` makes, so every serializer writes the same text.
        match self.value {
            Value::Object(object) => {
                if let Some(bytes) = self.binaries.binary(object).map_err(S::Error::custom)? {
                    // An index key is written as the decimal text the converted member's key holds.
                    let mut map = serializer.serialize_map(Some(bytes.len()))?;
                    for (index, byte) in bytes.iter().enumerate() {
                        map.serialize_entry(&index, byte)?;
                    }
                    return map.end();
                }
                let mut map = serializer.serialize_map(Some(object.len()))?;
                for (key, value) in object {
                    map.serialize_entry(key, &self.binaries.expanded(value))?;
                }
                map.end()
            }
            Value::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&self.binaries.expanded(value))?;
                }
                sequence.end()
            }
            value => value.serialize(serializer),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bytes(length: usize, seed: usize) -> Vec<u8> {
        (0..length).map(|index| ((index * 31 + seed) % 256) as u8).collect()
    }

    fn converted(length: usize, seed: usize) -> Value {
        Value::Object(bytes(length, seed).into_iter().enumerate().map(|(index, byte)| (index.to_string(), Value::from(byte))).collect())
    }

    fn document(a: Value, b: Value, c: Value) -> Value {
        // Keys out of encoding order, numbers the two writers print
        // differently, escapes, an empty binary and one binary twice.
        json!({
            "role": "user", "data": "line\n\"quoted\" \u{1}", "10": 1, "2": [a.clone(), {"z": b, "a": -0.0}],
            "float": 1.5e300, "big": 18446744073709551615u64, "negative": -12, "empty": c, "again": a,
            "nested": {"y": [[{"x": null}]], "b": true}, "": "",
        })
    }

    #[test]
    fn placeholders_write_what_their_converted_objects_write() {
        let mut binaries = HeldBinaries::new("test");
        let a = binaries.hold(bytes(300, 1));
        let b = binaries.hold(bytes(12, 2));
        let c = binaries.hold(Vec::new());
        let held = document(a, b, c);
        let expanded = document(converted(300, 1), converted(12, 2), converted(0, 0));
        assert_eq!(binaries.expand(&held).unwrap(), expanded);
        assert_eq!(binaries.json(&held).unwrap(), serde_json::to_string(&expanded).unwrap());
        assert_eq!(serde_json::to_vec_pretty(&binaries.expanded(&held)).unwrap(), serde_json::to_vec_pretty(&expanded).unwrap());
        assert_eq!(binaries.canonical(&held).unwrap(), payload_value::encode(&expanded).unwrap());
    }

    #[test]
    fn a_placeholder_meets_the_depth_limit_where_its_object_does() {
        let mut binaries = HeldBinaries::new("test");
        let placeholder = binaries.hold(bytes(3, 0));
        for depth in [payload_value::DEPTH_LIMIT - 1, payload_value::DEPTH_LIMIT] {
            let (mut held, mut expanded) = (placeholder.clone(), converted(3, 0));
            for _ in 0..depth {
                held = json!([held]);
                expanded = json!([expanded]);
            }
            let encoded = payload_value::encode(&expanded).ok();
            assert_eq!(binaries.canonical(&held).ok(), encoded, "{depth}");
            assert_eq!(encoded.is_some(), depth < payload_value::DEPTH_LIMIT);
        }
    }

    #[test]
    fn only_a_held_placeholder_is_written_as_bytes() {
        let mut binaries = HeldBinaries::new("test");
        let first = binaries.hold(bytes(3, 0));
        let second = binaries.hold(bytes(4, 1));
        // A second member, or no held bytes, leaves an object as it is.
        let key = first.as_object().unwrap().keys().next().unwrap().clone();
        let plain = json!({ &key: 0, "id": "kept" });
        assert_eq!(binaries.json(&plain).unwrap(), serde_json::to_string(&plain).unwrap());
        assert_eq!(HeldBinaries::default().json(&first).unwrap(), serde_json::to_string(&first).unwrap());
        assert_eq!(HeldBinaries::default().canonical(&first).unwrap(), payload_value::encode(&first).unwrap());
        // Released bytes fail a write instead of leaving the placeholder in place.
        binaries.release_before(1);
        assert!(binaries.json(&json!([first.clone()])).is_err());
        assert!(binaries.canonical(&first).is_err());
        assert!(binaries.expand(&first).is_err());
        assert_eq!(binaries.json(&second).unwrap(), serde_json::to_string(&converted(4, 1)).unwrap());
        // Bytes held after a truncation take the dropped indexes.
        binaries.truncate(1);
        let third = binaries.hold(bytes(2, 2));
        assert_eq!(third, second);
        assert_eq!(binaries.canonical(&third).unwrap(), payload_value::encode(&converted(2, 2)).unwrap());
    }
}
