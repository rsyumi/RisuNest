//! The cold item keys a block RisuSave makes staging read, in the order staging
//! reads them, so an official snapshot restore can read the items ahead. A key
//! is named only where staging is certain to read it; a key that depends on an
//! item not read yet is left to staging's own lookup.
use super::cold_plugin_storage::{INDEX_KEY, LEGACY_MAP_KEY};
use super::{upstream_import, RISU_SAVE_HEADER};
use flate2::read::GzDecoder;
use serde::de::{self, DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, BufReader, Read};

const COLD_MARKER: &str = "\u{ef01}COLDSTORAGE\u{ef01}";
/// Longer keys are not named; staging still reads them itself.
const MAX_KEY_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColdKey {
    pub(crate) key: String,
    /// A character item, whose chats name further items.
    pub(crate) character: bool,
}

/// Passes each key until `emit` returns false or the source ends, and passes
/// the offset of each block it begins once every key before the block is
/// passed. A source it cannot read ends the scan; staging reports the damage itself.
pub(crate) fn scan(source: impl Read, max_decoded_block_bytes: u64, emit: &mut dyn FnMut(ColdKey) -> bool, begun: &mut dyn FnMut(u64)) {
    let _ = scan_blocks(source, max_decoded_block_bytes, emit, begun);
}

/// Checks that a cold character item is one JSON value, as any item must be,
/// and returns the chat keys staging reads once it expands the character.
pub(crate) fn character_payload_keys(source: impl Read) -> serde_json::Result<Vec<String>> {
    let mut deserializer = serde_json::Deserializer::from_reader(source);
    let character = EnvelopeSeed.deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(match character {
        Some(character) if !character.excluded() => character.chats.into_iter().filter_map(Text::into_key).collect(),
        _ => Vec::new(),
    })
}

fn scan_blocks(mut source: impl Read, limit: u64, emit: &mut dyn FnMut(ColdKey) -> bool, begun: &mut dyn FnMut(u64)) -> Option<()> {
    let mut header = [0u8; RISU_SAVE_HEADER.len()];
    source.read_exact(&mut header).ok()?;
    if header != RISU_SAVE_HEADER {
        return None;
    }
    let mut storage_block = None;
    let mut root_storage = None;
    let mut offset = header.len() as u64;
    loop {
        let mut prefix = [0u8; 3];
        if source.read(&mut prefix[..1]).ok()? == 0 {
            break;
        }
        // Plugin keys follow the last block, so the scan passes no offset for the end.
        begun(offset);
        source.read_exact(&mut prefix[1..]).ok()?;
        let [block_type, compression, name_length] = prefix;
        let mut name = vec![0u8; usize::from(name_length)];
        source.read_exact(&mut name).ok()?;
        let mut length = [0u8; 4];
        source.read_exact(&mut length).ok()?;
        let length = u64::from(u32::from_le_bytes(length));
        offset += 3 + u64::from(name_length) + 4 + length;
        let mut body = (&mut source).take(length);
        match (block_type, name.as_slice()) {
            (2 | 7, _) => {
                let Some(character) = decode(&mut body, compression, limit, CharacterSeed)? else { return None };
                if !character.excluded() {
                    let keys = match character.cold {
                        // The expanded character replaces the stub's chats.
                        Text::Key(key) if !key.is_empty() => vec![ColdKey { key, character: true }],
                        Text::Long => Vec::new(),
                        _ => character.chats.into_iter().filter_map(Text::into_key).map(|key| ColdKey { key, character: false }).collect(),
                    };
                    for key in keys {
                        if !emit(key) {
                            return None;
                        }
                    }
                }
            }
            // Plugin values are read after every character, from the block or else the root.
            (11, b"pluginStorage") => storage_block = Some(decode(&mut body, compression, limit, StorageSeed)?),
            (1, b"root") => root_storage = decode(&mut body, compression, limit, RootSeed)?,
            _ => {}
        }
        io::copy(&mut body, &mut io::sink()).ok()?;
        if body.limit() != 0 {
            return None;
        }
    }
    let storage = match storage_block {
        Some(storage) => storage,
        None => root_storage,
    };
    for key in storage.map(PluginKeys::keys).unwrap_or_default() {
        if !emit(ColdKey { key, character: false }) {
            return None;
        }
    }
    Some(())
}

fn decode<T>(body: &mut impl Read, compression: u8, limit: u64, seed: impl for<'de> DeserializeSeed<'de, Value = T>) -> Option<T> {
    let decoded: Box<dyn Read + '_> = match compression {
        0 => Box::new(body.take(limit)),
        1 => Box::new(GzDecoder::new(body).take(limit)),
        _ => return None,
    };
    let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(decoded));
    let value = seed.deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    Some(value)
}

/// A string member as staging reads it: absent or not a string, a key, or a
/// string too long to name.
#[derive(Debug, Default)]
enum Text {
    #[default]
    Other,
    Key(String),
    Long,
}

impl Text {
    fn as_str(&self) -> Option<&str> {
        match self {
            Text::Key(text) => Some(text),
            _ => None,
        }
    }

    fn into_key(self) -> Option<String> {
        match self {
            Text::Key(key) => Some(key),
            _ => None,
        }
    }

    fn of(text: &str) -> Self {
        if text.len() > MAX_KEY_BYTES { Text::Long } else { Text::Key(text.to_owned()) }
    }
}

/// Answers every JSON value that is not the one a visitor reads with its default.
macro_rules! other_values {
    ($($skip:ident),*) => {
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self::Value, E> { Ok(Default::default()) }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self::Value, E> { Ok(Default::default()) }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self::Value, E> { Ok(Default::default()) }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self::Value, E> { Ok(Default::default()) }
        fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> { Ok(Default::default()) }
        $(other_values!(@$skip);)*
    };
    (@str) => {
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Self::Value, E> { Ok(Default::default()) }
    };
    (@seq) => {
        fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
            while sequence.next_element::<IgnoredAny>()?.is_some() {}
            Ok(Default::default())
        }
    };
    (@map) => {
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
            while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
            Ok(Default::default())
        }
    };
}

/// Implements `DeserializeSeed` for a visitor that reads any JSON value.
macro_rules! any_value_seed {
    ($($seed:ident),*) => {$(
        impl<'de> DeserializeSeed<'de> for $seed {
            type Value = <Self as Visitor<'de>>::Value;
            fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
                deserializer.deserialize_any(self)
            }
        }
    )*};
}

any_value_seed!(TextSeed, MarkerSeed, EnvelopeSeed, CharacterSeed, ChatsSeed, ConversationSeed, MessagesSeed, MessageSeed, RootSeed, StorageSeed, LegacySeed);

/// Which of the given member names a key is, without copying it.
struct FieldSeed(&'static [&'static str]);

impl<'de> DeserializeSeed<'de> for FieldSeed {
    type Value = Option<usize>;
    fn deserialize<D: de::Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl<'de> Visitor<'de> for FieldSeed {
    type Value = Option<usize>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("a member name") }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(self.0.iter().position(|name| *name == value))
    }
}

struct TextSeed;

impl<'de> Visitor<'de> for TextSeed {
    type Value = Text;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("any JSON value") }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> { Ok(Text::of(value)) }
    other_values!(seq, map);
}

/// A message's `data`, when it is a cold chat marker.
struct MarkerSeed;

impl<'de> Visitor<'de> for MarkerSeed {
    type Value = Text;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("any JSON value") }
    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(value.strip_prefix(COLD_MARKER).map_or(Text::Other, Text::of))
    }
    other_values!(seq, map);
}

#[derive(Default)]
struct CharacterKeys {
    kind: Text,
    id: Text,
    cold: Text,
    chats: Vec<Text>,
}

impl CharacterKeys {
    fn excluded(&self) -> bool {
        upstream_import::excluded_character(self.kind.as_str(), self.id.as_str())
    }
}

/// A cold character item: staging expands the last `character` member.
struct EnvelopeSeed;

impl<'de> Visitor<'de> for EnvelopeSeed {
    type Value = Option<CharacterKeys>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("any JSON value") }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut character = None;
        while let Some(field) = map.next_key_seed(FieldSeed(&["character"]))? {
            match field {
                Some(_) => character = map.next_value_seed(CharacterSeed)?,
                None => { map.next_value::<IgnoredAny>()?; }
            }
        }
        Ok(character)
    }
    other_values!(str, seq);
}

/// A character record. A repeated member counts as its last occurrence, as staging reads it.
struct CharacterSeed;

impl<'de> Visitor<'de> for CharacterSeed {
    type Value = Option<CharacterKeys>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("any JSON value") }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut keys = CharacterKeys::default();
        while let Some(field) = map.next_key_seed(FieldSeed(&["type", "chaId", "coldstorage", "chats"]))? {
            match field {
                Some(0) => keys.kind = map.next_value_seed(TextSeed)?,
                Some(1) => keys.id = map.next_value_seed(TextSeed)?,
                Some(2) => keys.cold = map.next_value_seed(TextSeed)?,
                Some(_) => keys.chats = map.next_value_seed(ChatsSeed)?,
                None => { map.next_value::<IgnoredAny>()?; }
            }
        }
        Ok(Some(keys))
    }
    other_values!(str, seq);
}

struct ChatsSeed;

impl<'de> Visitor<'de> for ChatsSeed {
    type Value = Vec<Text>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("any JSON value") }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let mut keys = Vec::new();
        while let Some(key) = sequence.next_element_seed(ConversationSeed)? {
            if !matches!(key, Text::Other) {
                keys.push(key);
            }
        }
        Ok(keys)
    }
    other_values!(str, map);
}

/// A chat whose first message is a cold marker; a repeated `message` counts as its last occurrence.
struct ConversationSeed;

impl<'de> Visitor<'de> for ConversationSeed {
    type Value = Text;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("any JSON value") }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut key = Text::Other;
        while let Some(field) = map.next_key_seed(FieldSeed(&["message"]))? {
            match field {
                Some(_) => key = map.next_value_seed(MessagesSeed)?,
                None => { map.next_value::<IgnoredAny>()?; }
            }
        }
        Ok(key)
    }
    other_values!(str, seq);
}

struct MessagesSeed;

impl<'de> Visitor<'de> for MessagesSeed {
    type Value = Text;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("any JSON value") }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Self::Value, A::Error> {
        let key = sequence.next_element_seed(MessageSeed)?.unwrap_or_default();
        while sequence.next_element::<IgnoredAny>()?.is_some() {}
        Ok(key)
    }
    other_values!(str, map);
}

struct MessageSeed;

impl<'de> Visitor<'de> for MessageSeed {
    type Value = Text;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("any JSON value") }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut key = Text::Other;
        while let Some(field) = map.next_key_seed(FieldSeed(&["data"]))? {
            match field {
                Some(_) => key = map.next_value_seed(MarkerSeed)?,
                None => { map.next_value::<IgnoredAny>()?; }
            }
        }
        Ok(key)
    }
    other_values!(str, seq);
}

#[derive(Default)]
struct PluginKeys {
    index: Text,
    legacy: Option<Vec<(String, Text)>>,
    inline: HashSet<String>,
}

impl PluginKeys {
    /// The index item first. The values it lists, and which legacy values it
    /// replaces, are known only once it is read, so with an index nothing else is named.
    fn keys(self) -> Vec<String> {
        match self.index {
            Text::Key(id) => return vec![id],
            Text::Long => return Vec::new(),
            Text::Other => {}
        }
        let inline = self.inline;
        self.legacy
            .unwrap_or_default()
            .into_iter()
            .filter(|(key, _)| key != INDEX_KEY && key != LEGACY_MAP_KEY && !inline.contains(key))
            .filter_map(|(_, id)| id.into_key())
            .collect()
    }
}

struct RootSeed;

impl<'de> Visitor<'de> for RootSeed {
    type Value = Option<PluginKeys>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("any JSON value") }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut storage = None;
        while let Some(field) = map.next_key_seed(FieldSeed(&["pluginCustomStorage"]))? {
            match field {
                Some(_) => storage = map.next_value_seed(StorageSeed)?,
                None => { map.next_value::<IgnoredAny>()?; }
            }
        }
        Ok(storage)
    }
    other_values!(str, seq);
}

struct StorageSeed;

impl<'de> Visitor<'de> for StorageSeed {
    type Value = Option<PluginKeys>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("any JSON value") }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut keys = PluginKeys::default();
        while let Some(key) = map.next_key::<String>()? {
            if key == INDEX_KEY {
                keys.index = map.next_value_seed(TextSeed)?;
            } else if key == LEGACY_MAP_KEY {
                keys.legacy = map.next_value_seed(LegacySeed)?;
            } else {
                map.next_value::<IgnoredAny>()?;
                keys.inline.insert(key);
            }
        }
        Ok(Some(keys))
    }
    other_values!(str, seq);
}

/// The legacy map in staging's order: a repeated key keeps its first place and its last value.
struct LegacySeed;

impl<'de> Visitor<'de> for LegacySeed {
    type Value = Option<Vec<(String, Text)>>;
    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result { formatter.write_str("any JSON value") }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut entries: Vec<(String, Text)> = Vec::new();
        let mut positions: HashMap<String, usize> = HashMap::new();
        while let Some(key) = map.next_key::<String>()? {
            let id = map.next_value_seed(TextSeed)?;
            match positions.get(&key) {
                Some(&position) => entries[position].1 = id,
                None => {
                    positions.insert(key.clone(), entries.len());
                    entries.push((key, id));
                }
            }
        }
        Ok(Some(entries))
    }
    other_values!(str, seq);
}
