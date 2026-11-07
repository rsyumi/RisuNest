//! Semantic comparison encoding for locally authored JavaScript JSON values.
use crate::{Result, WireError};
use serde::{de::{self, MapAccess, SeqAccess, Visitor}, Deserialize, Deserializer};
use serde_json::{Map, Value};
use std::{cmp::Ordering, fmt, io::Write};

struct Strict(Value);
impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct JsonVisitor;
        impl<'de> Visitor<'de> for JsonVisitor {
            type Value = Strict;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result { f.write_str("finite JavaScript JSON without duplicate keys") }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Strict, E> { Ok(Strict(Value::Bool(v))) }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Strict, E> { Ok(Strict(Value::Null)) }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Strict, E> { Ok(Strict(Value::String(v.into()))) }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Strict, E> { Ok(Strict(Value::String(v))) }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Strict, E> { self.visit_f64(v as f64) }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Strict, E> { self.visit_f64(v as f64) }
            fn visit_f64<E: de::Error>(self, v: f64) -> std::result::Result<Strict, E> {
                serde_json::Number::from_f64(v).map(|n| Strict(Value::Number(n))).ok_or_else(|| E::custom("non-finite-number"))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> std::result::Result<Strict, A::Error> {
                let mut values = Vec::new();
                while let Some(Strict(value)) = a.next_element()? { values.push(value); }
                Ok(Strict(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> std::result::Result<Strict, A::Error> {
                let mut values = Map::new();
                while let Some(key) = a.next_key::<String>()? {
                    if values.contains_key(&key) { return Err(de::Error::custom("duplicate-key")); }
                    let Strict(value) = a.next_value()?;
                    values.insert(key, value);
                }
                Ok(Strict(Value::Object(values)))
            }
        }
        d.deserialize_any(JsonVisitor)
    }
}
pub fn canonicalize(bytes: &[u8]) -> Result<Vec<u8>> {
    encode(&parse(bytes)?)
}
/// The value `canonicalize` encodes, with every number read as a JavaScript number.
pub fn parse(bytes: &[u8]) -> Result<Value> {
    let Strict(value) = serde_json::from_slice(bytes).map_err(|_| WireError("invalid-payload-json"))?;
    Ok(value)
}
pub fn encode(value: &Value) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write(value, &mut out, 0)?;
    Ok(out)
}
/// Compare without retaining a second encoded body. Parsing still rejects
/// duplicate keys, invalid numbers, and values beyond the JSON nesting limit.
pub fn is_canonical(bytes: &[u8]) -> bool {
    struct Compare<'a>(&'a [u8]);
    impl Write for Compare<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if !self.0.starts_with(bytes) { return Err(std::io::ErrorKind::InvalidData.into()); }
            self.0 = &self.0[bytes.len()..];
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }
    let Ok(value) = parse(bytes) else { return false; };
    let mut compare = Compare(bytes);
    write(&value, &mut compare, 0).is_ok() && compare.0.is_empty()
}
/// Objects and arrays at this depth or deeper are not encoded.
pub const DEPTH_LIMIT: usize = 127;
/// A JavaScript array index: the decimal text of an integer below `u32::MAX`
/// without a sign or leading zeros.
fn array_index(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.len() > 10 || (bytes[0] == b'0' && bytes.len() > 1) || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let value = key.parse::<u64>().ok()?;
    (value < u64::from(u32::MAX)).then_some(value as u32)
}
/// The order of object keys in an encoding: array indexes by value, then the
/// other keys by UTF-16 code units, as JavaScript orders own properties.
pub fn key_order(a: &str, b: &str) -> Ordering {
    match (array_index(a), array_index(b)) {
        (Some(a), Some(b)) => a.cmp(&b), (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater, _ => a.encode_utf16().cmp(b.encode_utf16()),
    }
}
/// Writes a number as JavaScript prints it, with `-0` as `0`.
pub fn write_number(number: f64, out: &mut impl Write) -> Result<()> {
    if !number.is_finite() { return Err(WireError("unsupported-payload-number")); }
    out.write_all(ryu_js::Buffer::new().format(if number == 0.0 { 0.0 } else { number }).as_bytes())
        .map_err(|_| WireError("invalid-payload-json"))
}
fn write(value: &Value, out: &mut impl Write, depth: usize) -> Result<()> {
    let io = |_| WireError("invalid-payload-json");
    if matches!(value, Value::Object(_) | Value::Array(_)) && depth >= DEPTH_LIMIT {
        return Err(WireError("invalid-payload-json"));
    }
    match value {
        Value::Object(values) => {
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_by(|a, b| key_order(a, b));
            out.write_all(b"{").map_err(io)?;
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 { out.write_all(b",").map_err(io)?; }
                serde_json::to_writer(&mut *out, key).map_err(|_| WireError("invalid-payload-json"))?;
                out.write_all(b":").map_err(io)?; write(&values[key], out, depth + 1)?;
            }
            out.write_all(b"}").map_err(io)?;
        }
        Value::Array(values) => {
            out.write_all(b"[").map_err(io)?;
            for (i, value) in values.iter().enumerate() {
                if i > 0 { out.write_all(b",").map_err(io)?; }
                write(value, out, depth + 1)?;
            }
            out.write_all(b"]").map_err(io)?;
        }
        Value::Number(number) => {
            write_number(number.as_f64().ok_or(WireError("unsupported-payload-number"))?, out)?;
        }
        _ => serde_json::to_writer(out, value).map_err(|_| WireError("invalid-payload-json"))?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn array_index_reads_the_keys_a_decimal_round_trip_reads() {
        fn round_trip(key: &str) -> Option<u32> {
            let value = key.parse::<u32>().ok()?;
            (value < u32::MAX && value.to_string() == key).then_some(value)
        }
        let mut keys: Vec<String> = [
            "", "0", "00", "01", "1", "9", "10", "+1", "-0", "-1", " 1", "1 ", "1.0", "1e3", "0x1",
            "4294967294", "4294967295", "4294967296", "04294967294", "9999999999", "99999999999",
            "18446744073709551615", "18446744073709551616", "\u{0661}", "１", "a", "id",
        ].iter().map(|key| (*key).to_owned()).collect();
        keys.extend((0..2_000u32).map(|index| index.to_string()));
        for key in &keys {
            assert_eq!(array_index(key), round_trip(key), "{key:?}");
        }
    }
}
