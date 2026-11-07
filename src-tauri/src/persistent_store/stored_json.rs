//! Reads stored JSON text one value at a time and writes it as the parsed
//! `serde_json::Value` would be written, without building that value. A
//! converted `Uint8Array` is an object with one member per byte, so a tree of it
//! costs about two hundred bytes per byte while its text costs about twelve.
//!
//! Every result matches the value its decoder reads: the text is checked as a
//! parse checks it, a repeated object key keeps its first position and its last
//! value, and numbers are read as the parse reads them.

use super::{lww, StoreError, StoreResult};
use risunest_sync_wire::payload_value;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::value::RawValue;
use serde_json::Value;
use std::borrow::Cow;
use std::cell::Cell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

/// The nesting a stored text's decoder accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Depth {
    /// `serde_json::from_str`, which fails past 128 levels.
    Limited,
    /// A decoder whose recursion limit is disabled.
    Unlimited,
}

/// With serde_json's `raw_value` feature, a `Value` reads an object whose first
/// key is this token as the JSON text that the member's string holds.
const RAW_VALUE_TOKEN: &str = "$serde_json::private::RawValue";

/// A stored JSON text that decodes as `Value`.
pub(super) struct Document<'a> {
    text: Cow<'a, str>,
}

impl<'a> Document<'a> {
    pub(super) fn parse(text: &'a str, depth: Depth) -> StoreResult<Self> {
        let token = Cell::new(false);
        match read_with(text, depth, Check { token: &token }) {
            Ok(()) => Ok(Self { text: Cow::Borrowed(text) }),
            // Such an object decodes to the value its string holds. Writing the
            // decoded value gives a text without one that decodes the same.
            Err(_) if token.get() => {
                let value: Value = read_with(text, depth, PhantomData)?;
                Ok(Self { text: Cow::Owned(serde_json::to_string(&value)?) })
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn value(&self) -> Raw<'_> {
        Raw(self.text.trim_matches(|c| matches!(c, ' ' | '\t' | '\n' | '\r')))
    }
}

/// One value of a `Document`.
#[derive(Clone, Copy, Debug)]
pub(super) struct Raw<'a>(&'a str);

/// The members of an object as a parse keeps them.
pub(super) struct Object<'a> {
    members: Vec<(Cow<'a, str>, Raw<'a>)>,
}

impl<'a> Raw<'a> {
    fn first(self) -> u8 {
        self.0.as_bytes()[0]
    }

    pub(super) fn is_array(self) -> bool {
        self.first() == b'['
    }

    /// The string this value is, or `None` for any other value.
    pub(super) fn as_str(self) -> StoreResult<Option<Cow<'a, str>>> {
        if self.first() != b'"' {
            return Ok(None);
        }
        read(self.0, Text).map(Some)
    }

    /// The members of an object, or `None` for any other value.
    pub(super) fn object(self) -> StoreResult<Option<Object<'a>>> {
        if self.first() != b'{' {
            return Ok(None);
        }
        read(self.0, Members).map(Some)
    }

    /// The elements of an array, or `None` for any other value.
    pub(super) fn elements(self) -> StoreResult<Option<Vec<Raw<'a>>>> {
        if !self.is_array() {
            return Ok(None);
        }
        read(self.0, Elements).map(Some)
    }

    /// Appends what `payload_value::encode` writes for the parsed value.
    pub(super) fn write_canonical(self, out: &mut Vec<u8>) -> StoreResult<()> {
        write(self, out, 0, Mode::Canonical)
    }

    /// Appends what `serde_json::to_writer` writes for the parsed value.
    pub(super) fn write_json(self, out: &mut Vec<u8>) -> StoreResult<()> {
        write(self, out, 0, Mode::Json)
    }

    /// The parsed value, for a value small enough to hold as one.
    pub(super) fn parse_value(self) -> StoreResult<Value> {
        read(self.0, PhantomData)
    }
}

impl<'a> Object<'a> {
    pub(super) fn get(&self, name: &str) -> Option<Raw<'a>> {
        self.members.iter().find(|(key, _)| key == name).map(|(_, value)| *value)
    }

    pub(super) fn members(&self) -> impl Iterator<Item = (&str, Raw<'a>)> + '_ {
        self.members.iter().map(|(key, value)| (key.as_ref(), *value))
    }
}

/// Appends what `payload_value::encode` writes for an object of `members`,
/// whose keys must be distinct.
pub(super) fn write_canonical_object<'k, 'r>(members: impl IntoIterator<Item = (&'k str, Raw<'r>)>, out: &mut Vec<u8>) -> StoreResult<()> {
    canonical_members(members.into_iter().collect(), out, 0)
}

/// Appends what serde_json writes for an object of `members`, whose keys must
/// be distinct, with each member's value written by `write_member`.
pub(super) fn write_json_object<K: AsRef<str>, V>(
    members: impl IntoIterator<Item = (K, V)>,
    out: &mut Vec<u8>,
    mut write_member: impl FnMut(&str, V, &mut Vec<u8>) -> StoreResult<()>,
) -> StoreResult<()> {
    out.push(b'{');
    for (index, (key, value)) in members.into_iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut *out, key.as_ref())?;
        out.push(b':');
        write_member(key.as_ref(), value, out)?;
    }
    out.push(b'}');
    Ok(())
}

/// Appends what serde_json writes for an array of `elements`.
pub(super) fn write_json_array<'r>(elements: impl IntoIterator<Item = Raw<'r>>, out: &mut Vec<u8>) -> StoreResult<()> {
    out.push(b'[');
    for (index, element) in elements.into_iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        element.write_json(out)?;
    }
    out.push(b']');
    Ok(())
}

/// The text of a written value.
pub(super) fn text(out: Vec<u8>) -> StoreResult<String> {
    String::from_utf8(out).map_err(|_| invalid_payload())
}

fn invalid_payload() -> StoreError {
    lww::error(risunest_sync_wire::WireError("invalid-payload-json"))
}

fn read<'a, S: DeserializeSeed<'a>>(text: &'a str, seed: S) -> StoreResult<S::Value> {
    // The document was checked as a whole, so its parts nest within its limit.
    read_with(text, Depth::Unlimited, seed)
}

fn read_with<'a, S: DeserializeSeed<'a>>(text: &'a str, depth: Depth, seed: S) -> StoreResult<S::Value> {
    let mut decoder = serde_json::Deserializer::from_str(text);
    if depth == Depth::Unlimited {
        decoder.disable_recursion_limit();
    }
    let value = seed.deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(value)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Canonical,
    Json,
}

#[derive(Default)]
enum Abort {
    #[default]
    None,
    /// An object's keys are not strictly ascending.
    Unordered,
    Failed(StoreError),
}

/// Writes `value` in one pass when every object in it has strictly ascending
/// keys, as a converted binary has; otherwise it writes the members of the
/// outermost object or array one at a time, each in a pass of its own.
fn write(value: Raw<'_>, out: &mut Vec<u8>, depth: usize, mode: Mode) -> StoreResult<()> {
    if value.first() == b'"' && !value.0.contains('\\') {
        // A string without escapes is already written as serde_json writes it.
        out.extend_from_slice(value.0.as_bytes());
        return Ok(());
    }
    let start = out.len();
    let abort = Cell::new(Abort::None);
    match read(value.0, Inline { out: &mut *out, depth, mode, abort: &abort }) {
        Ok(()) => return Ok(()),
        Err(error) => match abort.take() {
            Abort::None => return Err(error),
            Abort::Failed(failure) => return Err(failure),
            Abort::Unordered => out.truncate(start),
        },
    }
    if let Some(object) = value.object()? {
        let members: Vec<_> = object.members().collect();
        if mode == Mode::Canonical {
            return canonical_members(members, out, depth);
        }
        write_json_object(members, out, |_, member, out| write(member, out, depth + 1, mode))
    } else {
        out.push(b'[');
        for (index, element) in value.elements()?.into_iter().flatten().enumerate() {
            if index > 0 {
                out.push(b',');
            }
            write(element, out, depth + 1, mode)?;
        }
        out.push(b']');
        Ok(())
    }
}

fn canonical_members(mut members: Vec<(&str, Raw<'_>)>, out: &mut Vec<u8>, depth: usize) -> StoreResult<()> {
    if depth >= payload_value::DEPTH_LIMIT {
        return Err(invalid_payload());
    }
    members.sort_by(|a, b| payload_value::key_order(a.0, b.0));
    out.push(b'{');
    for (index, (key, member)) in members.into_iter().enumerate() {
        if index > 0 {
            out.push(b',');
        }
        serde_json::to_writer(&mut *out, key)?;
        out.push(b':');
        write(member, out, depth + 1, Mode::Canonical)?;
    }
    out.push(b'}');
    Ok(())
}

/// Checks a text as a `Value` parse would, and notes an object whose first key
/// is the raw value token.
struct Check<'c> {
    token: &'c Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for Check<'_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Check<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }
    fn visit_bool<E: de::Error>(self, _: bool) -> Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, _: i64) -> Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: de::Error>(self, _: u64) -> Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: de::Error>(self, _: f64) -> Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        Ok(())
    }
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<(), A::Error> {
        while values.next_element_seed(Check { token: self.token })?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<(), A::Error> {
        let mut first = true;
        while let Some(key) = entries.next_key_seed(Key)? {
            if first && key == RAW_VALUE_TOKEN {
                self.token.set(true);
                return Err(de::Error::custom("raw value token"));
            }
            first = false;
            entries.next_value_seed(Check { token: self.token })?;
        }
        Ok(())
    }
}

/// Writes one value, stopping with `Abort::Unordered` at an object whose keys
/// are not strictly ascending in `payload_value::key_order`. Ascending keys are
/// distinct, so a parse keeps them in text order, and they are already in the
/// order an encoding sorts them into.
struct Inline<'o, 'c> {
    out: &'o mut Vec<u8>,
    depth: usize,
    mode: Mode,
    abort: &'c Cell<Abort>,
}

impl Inline<'_, '_> {
    fn fail<E: de::Error>(&self, abort: Abort) -> E {
        self.abort.set(abort);
        E::custom("stored JSON write stopped")
    }

    fn number<E: de::Error>(self, number: f64, json: impl serde::Serialize) -> Result<(), E> {
        match self.mode {
            Mode::Canonical => payload_value::write_number(number, &mut *self.out)
                .map_err(|error| self.fail(Abort::Failed(lww::error(error)))),
            Mode::Json => serde_json::to_writer(&mut *self.out, &json).map_err(|error| self.fail(Abort::Failed(error.into()))),
        }
    }

    fn nested(&mut self) -> Inline<'_, '_> {
        Inline { out: &mut *self.out, depth: self.depth + 1, mode: self.mode, abort: self.abort }
    }

    fn enter<E: de::Error>(&self) -> Result<(), E> {
        if self.mode == Mode::Canonical && self.depth >= payload_value::DEPTH_LIMIT {
            return Err(self.fail(Abort::Failed(invalid_payload())));
        }
        Ok(())
    }
}

impl<'de> DeserializeSeed<'de> for Inline<'_, '_> {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Inline<'_, '_> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }
    fn visit_bool<E: de::Error>(self, value: bool) -> Result<(), E> {
        self.out.extend_from_slice(if value { b"true" } else { b"false" });
        Ok(())
    }
    fn visit_unit<E: de::Error>(self) -> Result<(), E> {
        self.out.extend_from_slice(b"null");
        Ok(())
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> Result<(), E> {
        self.number(value as f64, value)
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> Result<(), E> {
        self.number(value as f64, value)
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> Result<(), E> {
        self.number(value, value)
    }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<(), E> {
        serde_json::to_writer(&mut *self.out, value).map_err(|error| self.fail(Abort::Failed(error.into())))
    }
    fn visit_seq<A: SeqAccess<'de>>(mut self, mut values: A) -> Result<(), A::Error> {
        self.enter()?;
        self.out.push(b'[');
        let mut first = true;
        loop {
            let length = self.out.len();
            if !first {
                self.out.push(b',');
            }
            if values.next_element_seed(self.nested())?.is_none() {
                self.out.truncate(length);
                break;
            }
            first = false;
        }
        self.out.push(b']');
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(mut self, mut entries: A) -> Result<(), A::Error> {
        self.enter()?;
        self.out.push(b'{');
        let mut previous: Option<Cow<'de, str>> = None;
        while let Some(key) = entries.next_key_seed(Key)? {
            if let Some(previous) = &previous {
                if payload_value::key_order(previous, &key) != Ordering::Less {
                    return Err(self.fail(Abort::Unordered));
                }
                self.out.push(b',');
            }
            serde_json::to_writer(&mut *self.out, key.as_ref()).map_err(|error| self.fail(Abort::Failed(error.into())))?;
            self.out.push(b':');
            entries.next_value_seed(self.nested())?;
            previous = Some(key);
        }
        self.out.push(b'}');
        Ok(())
    }
}

struct Key;

impl<'de> DeserializeSeed<'de> for Key {
    type Value = Cow<'de, str>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for Key {
    type Value = Cow<'de, str>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an object key")
    }
    fn visit_borrowed_str<E: de::Error>(self, key: &'de str) -> Result<Self::Value, E> {
        Ok(Cow::Borrowed(key))
    }
    fn visit_str<E: de::Error>(self, key: &str) -> Result<Self::Value, E> {
        Ok(Cow::Owned(key.to_owned()))
    }
}

struct Text;

impl<'de> DeserializeSeed<'de> for Text {
    type Value = Cow<'de, str>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(Key)
    }
}

struct Members;

impl<'de> DeserializeSeed<'de> for Members {
    type Value = Object<'de>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_map(self)
    }
}

impl<'de> Visitor<'de> for Members {
    type Value = Object<'de>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an object")
    }
    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Self::Value, A::Error> {
        let mut members: Vec<(Cow<'de, str>, Raw<'de>)> = Vec::new();
        let mut positions: HashMap<Cow<'de, str>, usize> = HashMap::new();
        while let Some(key) = entries.next_key_seed(Key)? {
            let value = Raw(entries.next_value::<&'de RawValue>()?.get());
            match positions.get(&key) {
                Some(&position) => members[position].1 = value,
                None => {
                    positions.insert(key.clone(), members.len());
                    members.push((key, value));
                }
            }
        }
        Ok(Object { members })
    }
}

struct Elements;

impl<'de> DeserializeSeed<'de> for Elements {
    type Value = Vec<Raw<'de>>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for Elements {
    type Value = Vec<Raw<'de>>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an array")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<Self::Value, A::Error> {
        let mut elements = Vec::new();
        while let Some(value) = values.next_element::<&'de RawValue>()? {
            elements.push(Raw(value.get()));
        }
        Ok(elements)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    type Written = (StoreResult<Vec<u8>>, StoreResult<Vec<u8>>);

    /// What the parsed value encodes to and serializes to.
    fn parsed(text: &str, depth: Depth) -> Written {
        let parse = || -> StoreResult<Value> {
            let mut decoder = serde_json::Deserializer::from_str(text);
            if depth == Depth::Unlimited {
                decoder.disable_recursion_limit();
            }
            let value = Value::deserialize(&mut decoder)?;
            decoder.end()?;
            Ok(value)
        };
        match parse() {
            Ok(value) => (
                payload_value::encode(&value).map_err(lww::error),
                Ok(serde_json::to_vec(&value).unwrap()),
            ),
            Err(error) => (Err(error), parse().map(|_| Vec::new())),
        }
    }

    fn written(text: &str, depth: Depth) -> Written {
        let document = match Document::parse(text, depth) {
            Ok(document) => document,
            Err(error) => return (Err(error), Document::parse(text, depth).map(|_| Vec::new())),
        };
        let mut canonical = Vec::new();
        let mut json = Vec::new();
        (
            document.value().write_canonical(&mut canonical).map(|()| canonical),
            document.value().write_json(&mut json).map(|()| json),
        )
    }

    fn assert_same(text: &str) {
        for depth in [Depth::Limited, Depth::Unlimited] {
            assert_eq!(written(text, depth), parsed(text, depth), "{depth:?} {text}");
        }
    }

    fn nested(depth: usize, open: &str, close: &str) -> String {
        format!("{}0{}", open.repeat(depth), close.repeat(depth))
    }

    #[test]
    fn writes_what_the_parsed_value_writes() {
        let binary = (0..300).map(|index| format!("\"{index}\":{}", index % 256)).collect::<Vec<_>>().join(",");
        let shuffled = (0..300).rev().map(|index| format!("\"{index}\":{}", index % 256)).collect::<Vec<_>>().join(",");
        let mut texts: Vec<String> = vec![
            // Converted binaries, in ascending and other key orders.
            format!("{{{binary}}}"),
            format!("{{{shuffled}}}"),
            format!("{{\"id\":\"module\",\"binary\":{{{binary}}},\"assets\":[]}}"),
            format!("[{{{binary}}},{{{shuffled}}},{{}}]"),
            r#"{"0":1,"1":2,"10":3,"2":4}"#.into(),
            r#"{"0":1,"0":2}"#.into(),
            // Nesting and empty containers.
            "{}".into(),
            "[]".into(),
            r#"[[],{},[{}],{"a":[]}]"#.into(),
            r#"{"a":{"b":{"c":[1,[2,[3]]]}}}"#.into(),
            "  {\"a\" : [ 1 , 2 ] , \"b\" :\t{ } }\r\n".into(),
            // Numbers.
            "[0,-0,0.0,-0.0,1.0,1,-1,1.5,1e21,1e-7,1E5,2.5E-3,123e-2]".into(),
            "[18446744073709551615,18446744073709551616,-9223372036854775808,-9223372036854775809]".into(),
            "[9007199254740993,123456789012345678901234567890,5e-324,1.7976931348623157e308]".into(),
            r#"{"n":-0}"#.into(),
            "1e400".into(),
            "[1e400]".into(),
            "-".into(),
            // Strings and escapes.
            r#""plain""#.into(),
            r#""\n\t\"\\\/\u0000\u001f\u007f ""#.into(),
            "[\"\u{d55c}\u{ae00}\",\"\\ud83d\\ude00\",\"\u{1f600}\",\"/\"]".into(),
            r#""\ud800""#.into(),
            r#""é""#.into(),
            "[true,false,null]".into(),
            "12".into(),
            // Member order and repeated keys.
            "{\"b\":1,\"a\":2,\"10\":3,\"9\":4,\"01\":5,\"4294967295\":6,\"4294967294\":7,\"\":8,\"\u{e9}\":9,\"\\ud83d\\ude00\":10,\"\u{ff5a}\":11}".into(),
            r#"{"a":1,"b":2,"a":3}"#.into(),
            r#"{"a":{"x":1},"a":[2]}"#.into(),
            r#"{"a":1,"a":2}"#.into(),
            // A first member named by serde_json's raw value token.
            r#"{"$serde_json::private::RawValue":"{\"x\":[1,2],\"a\":-0}"}"#.into(),
            r#"[{"$serde_json::private::RawValue":"5"},{"b":{"$serde_json::private::RawValue":"[1e400]"}}]"#.into(),
            r#"{"a":1,"$serde_json::private::RawValue":"x"}"#.into(),
            r#"{"$serde_json::private::RawValue":5}"#.into(),
            r#"{"$serde_json::private::RawValue":"{}","b":1}"#.into(),
            r#"["\"$serde_json::private::RawValue\""]"#.into(),
            // Malformed text.
            r#"{"a":1,}"#.into(),
            "[1 2]".into(),
            r#"{"a":1} x"#.into(),
        ];
        for depth in [126, 127, 128, 129, 200] {
            texts.push(nested(depth, "[", "]"));
            texts.push(nested(depth, "{\"a\":", "}"));
        }
        for text in &texts {
            assert_same(text);
        }
    }

    struct Generator(u64);

    impl Generator {
        fn below(&mut self, bound: u64) -> u64 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (self.0 >> 33) % bound
        }

        fn pick<'a>(&mut self, values: &[&'a str]) -> &'a str {
            values[self.below(values.len() as u64) as usize]
        }

        fn value(&mut self, depth: usize, out: &mut String) {
            const NUMBERS: &[&str] = &["0", "-0", "7", "-12", "1.0", "-0.0", "3.25e2", "1e-7", "18446744073709551616", "255"];
            const STRINGS: &[&str] = &[r#""""#, r#""x""#, r#""\n""#, r#""\u0000""#, "\"\u{d55c}\"", r#""😀""#, r#""a\"b""#];
            const KEYS: &[&str] = &["0", "1", "2", "10", "9", "01", "a", "b", "id", "", "\u{e9}", r"😀", "\u{ff5a}", "4294967294", "4294967295"];
            const SPACE: &[&str] = &["", "", " ", "\n"];
            out.push_str(self.pick(SPACE));
            match self.below(if depth > 5 { 5 } else { 9 }) {
                0 => out.push_str(self.pick(&["null", "true", "false"])),
                1 | 2 => out.push_str(self.pick(NUMBERS)),
                3 | 4 => out.push_str(self.pick(STRINGS)),
                5 | 6 => {
                    out.push('{');
                    let count = self.below(6);
                    // Ascending numeric keys, as a converted binary has, or any keys.
                    let ascending = self.below(2) == 0;
                    for index in 0..count {
                        if index > 0 {
                            out.push(',');
                        }
                        let key = if ascending { index.to_string() } else { self.pick(KEYS).to_owned() };
                        out.push_str(&format!("\"{key}\":"));
                        self.value(depth + 1, out);
                    }
                    out.push('}');
                }
                _ => {
                    out.push('[');
                    for index in 0..self.below(5) {
                        if index > 0 {
                            out.push(',');
                        }
                        self.value(depth + 1, out);
                    }
                    out.push(']');
                }
            }
            out.push_str(self.pick(SPACE));
        }
    }

    #[test]
    fn generated_values_write_what_their_parsed_values_write() {
        let mut generator = Generator(0x5eed);
        for _ in 0..3000 {
            let mut text = String::new();
            generator.value(0, &mut text);
            assert_same(&text);
        }
    }

    #[test]
    fn reads_members_and_elements_as_the_parsed_value_holds_them() {
        let text = r#"{"b":[1,{"x":2}],"a":"A","b":[3],"c":{}}"#;
        let document = Document::parse(text, Depth::Limited).unwrap();
        let object = document.value().object().unwrap().unwrap();
        let members: Vec<_> = object.members().map(|(key, value)| (key.to_owned(), value.parse_value().unwrap())).collect();
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(members, value.as_object().unwrap().iter().map(|(key, value)| (key.clone(), value.clone())).collect::<Vec<_>>());
        assert_eq!(object.get("a").unwrap().as_str().unwrap().as_deref(), Some("A"));
        assert_eq!(object.get("c").unwrap().as_str().unwrap(), None);
        assert!(object.get("b").unwrap().is_array());
        assert_eq!(object.get("b").unwrap().elements().unwrap().unwrap().len(), 1);
        assert!(object.get("c").unwrap().elements().unwrap().is_none());
        assert!(document.value().elements().unwrap().is_none());
    }
}
