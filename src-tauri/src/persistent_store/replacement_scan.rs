//! Reads what a database-only replacement needs from stored JSON text without
//! building the value. Results match a parsed `serde_json::Value`: every value
//! is still decoded, so malformed text, out-of-range numbers and nesting past
//! the recursion limit fail as before, and a repeated object key keeps only its
//! last value.

use super::StoreResult;
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::fmt;

/// Whether `text` is an object whose `additionalAssets`, when present, is an array.
pub(super) fn additional_assets_are_listed(text: &str) -> StoreResult<bool> {
    let mut decoder = serde_json::Deserializer::from_str(text);
    let listed = AdditionalAssets.deserialize(&mut decoder)?;
    decoder.end()?;
    Ok(listed)
}

/// Adds the asset keys and Inlay ids named by string values in `text`.
pub(super) fn observe_alias_references(
    text: &str,
    unbounded_depth: bool,
    references: &mut BTreeSet<String>,
) -> StoreResult<()> {
    let mut decoder = serde_json::Deserializer::from_str(text);
    if unbounded_depth {
        decoder.disable_recursion_limit();
    }
    let found = References.deserialize(&mut decoder)?;
    decoder.end()?;
    references.extend(found);
    Ok(())
}

pub(super) fn observe_alias_text(value: &str, references: &mut impl Extend<String>) {
    if value.starts_with("assets/") {
        references.extend([value.to_owned()]);
    }
    for prefix in ["{{inlay::", "{{inlayed::", "{{inlayeddata::"] {
        let mut remainder = value;
        while let Some(start) = remainder.find(prefix) {
            remainder = &remainder[start + prefix.len()..];
            let Some(end) = remainder.find("}}") else {
                break;
            };
            references.extend([remainder[..end].to_owned()]);
            remainder = &remainder[end + 2..];
        }
    }
}

macro_rules! scalars {
    ($value:expr) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> {
            Ok($value)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
            Ok($value)
        }
    };
}

/// Decodes and discards one value. `IgnoredAny` would skip the checks a parse makes.
struct Skip;

impl<'de> DeserializeSeed<'de> for Skip {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Skip {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }
    scalars!(());
    fn visit_str<E: de::Error>(self, _: &str) -> Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<(), A::Error> {
        while values.next_element_seed(Skip)?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<(), A::Error> {
        while entries.next_key_seed(Skip)?.is_some() {
            entries.next_value_seed(Skip)?;
        }
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

struct IsArray;

impl<'de> DeserializeSeed<'de> for IsArray {
    type Value = bool;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<bool, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for IsArray {
    type Value = bool;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }
    scalars!(false);
    fn visit_str<E: de::Error>(self, _: &str) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, values: A) -> Result<bool, A::Error> {
        Skip.visit_seq(values).map(|()| true)
    }
    fn visit_map<A: MapAccess<'de>>(self, entries: A) -> Result<bool, A::Error> {
        Skip.visit_map(entries).map(|()| false)
    }
}

struct AdditionalAssets;

impl<'de> DeserializeSeed<'de> for AdditionalAssets {
    type Value = bool;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<bool, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for AdditionalAssets {
    type Value = bool;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a character detail")
    }
    scalars!(false);
    fn visit_str<E: de::Error>(self, _: &str) -> Result<bool, E> {
        Ok(false)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, values: A) -> Result<bool, A::Error> {
        Skip.visit_seq(values).map(|()| false)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<bool, A::Error> {
        let mut listed = true;
        while let Some(key) = entries.next_key_seed(Key)? {
            if key == "additionalAssets" {
                listed = entries.next_value_seed(IsArray)?;
            } else {
                entries.next_value_seed(Skip)?;
            }
        }
        Ok(listed)
    }
}

struct References;

impl<'de> DeserializeSeed<'de> for References {
    type Value = Vec<String>;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Vec<String>, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for References {
    type Value = Vec<String>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }
    scalars!(Vec::new());
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Vec<String>, E> {
        let mut found = Vec::new();
        observe_alias_text(value, &mut found);
        Ok(found)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut values: A) -> Result<Vec<String>, A::Error> {
        let mut found = Vec::new();
        while let Some(value) = values.next_element_seed(References)? {
            found.extend(value);
        }
        Ok(found)
    }
    fn visit_map<A: MapAccess<'de>>(self, mut entries: A) -> Result<Vec<String>, A::Error> {
        // Only entries that name something are kept, keyed so a repeated key
        // replaces what its earlier value named.
        let mut found = HashMap::<Cow<'de, str>, Vec<String>>::new();
        while let Some(key) = entries.next_key_seed(Key)? {
            let value = entries.next_value_seed(References)?;
            if !value.is_empty() {
                found.insert(key, value);
            } else if !found.is_empty() {
                found.remove(&key);
            }
        }
        Ok(found.into_values().flatten().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn references(text: &str) -> StoreResult<BTreeSet<String>> {
        let mut found = BTreeSet::new();
        observe_alias_references(text, false, &mut found)?;
        Ok(found)
    }

    fn set(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn alias_references_come_from_string_values_at_any_depth() {
        let binary = (0..300).map(|index| format!("\"{index}\":{}", index % 256)).collect::<Vec<_>>().join(",");
        let text = [
            r#"{"name":"assets/top.png","assets/key-only.png":1,"late":1,"#,
            r#""emotionImages":[["happy","assets/nested.png"],[{"deep":{"deeper":["assets/escaped.png"]}}]],"#,
            r#""description":"x {{inlay::one}} y {{inlayed::two}} {{inlayeddata::three}} {{inlay::one}} {{inlay::open","#,
            r#""prefix":" assets/not-a-prefix.png","values":[null,true,-1,2.5e3,"plain"],"#,
            r#""replaced":"assets/first.png","kept":"assets/kept.png","replaced":{"inner":"assets/second.png"},"#,
            r#""emptied":"assets/emptied.png","emptied":7,"late":"assets/late.png","inlay":"{{inlay::escaped-key}}","#,
            &format!(r#""binary":{{{binary}}}}}"#),
        ]
        .concat();
        assert_eq!(
            references(&text).unwrap(),
            set(&[
                "assets/top.png", "assets/nested.png", "assets/escaped.png", "one", "two", "three",
                "assets/second.png", "assets/kept.png", "assets/late.png", "escaped-key",
            ])
        );
        assert_eq!(references(r#""assets/scalar.png""#).unwrap(), set(&["assets/scalar.png"]));
        assert_eq!(references(r#"["{{inlay::a}}{{inlay::b}}"]"#).unwrap(), set(&["a", "b"]));
        for empty in ["7", "null", "{}", "[]", r#"{"a":{"b":[1,{"c":false}]}}"#] {
            assert!(references(empty).unwrap().is_empty(), "{empty}");
        }
    }

    #[test]
    fn root_values_read_past_the_default_depth_like_their_decoder() {
        let deep = format!("{}\"assets/deep.png\"{}", "[".repeat(200), "]".repeat(200));
        assert!(references(&deep).is_err());
        let mut found = BTreeSet::new();
        observe_alias_references(&deep, true, &mut found).unwrap();
        assert_eq!(found, set(&["assets/deep.png"]));
    }

    #[test]
    fn additional_assets_shape_follows_the_parsed_detail() {
        for (text, listed) in [
            (r#"{"name":"x"}"#, true),
            (r#"{"additionalAssets":[]}"#, true),
            (r#"{"additionalAssets":[["a","assets/a.png","png"],{"odd":true},7]}"#, true),
            (r#"{"additionalAssets":null}"#, false),
            (r#"{"additionalAssets":{}}"#, false),
            (r#"{"additionalAssets":"assets/a.png"}"#, false),
            (r#"{"additionalAssets":{},"additionalAssets":[]}"#, true),
            (r#"{"additionalAssets":[],"additionalAssets":0}"#, false),
            (r#"{"additionalAssets":{}}"#, false),
            (r#"{"nested":{"additionalAssets":{}},"list":[{"additionalAssets":7}]}"#, true),
            (r#"[{"additionalAssets":[]}]"#, false),
            ("null", false),
            (r#""text""#, false),
            ("3", false),
        ] {
            assert_eq!(additional_assets_are_listed(text).unwrap(), listed, "{text}");
        }
    }

    #[test]
    fn malformed_text_fails_as_a_parse_would() {
        let deep = format!("{{\"a\":{}1{}}}", "[".repeat(200), "]".repeat(200));
        for text in [r#"{"a":1} trailing"#, r#"{"a":"#, "", r#"{"a":1e400}"#, r#"{"a":"\ud800"}"#, deep.as_str()] {
            assert!(serde_json::from_str::<serde_json::Value>(text).is_err(), "{text}");
            assert!(additional_assets_are_listed(text).is_err(), "{text}");
            assert!(references(text).is_err(), "{text}");
        }
    }
}
