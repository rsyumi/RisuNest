//! Logical section codec. Sections carry sorted logical keys, their values and
//! object references. The codec promises nothing about any local SQLite schema,
//! so a device can change its tables without changing what it publishes.
use super::{content_identity::hash, FormatError, Result};
use risunest_sync_wire::head::Sequence;
use serde::{Deserialize, Serialize};

pub const SECTION_CODEC: &str = "risunest.section-codec/v1";

/// Values larger than this are published as their own object instead of riding
/// inline in the entry.
pub const MAX_INLINE_VALUE_BYTES: usize = 4 * 1024;

pub const MAX_SECTION_ENTRY_BYTES: usize = 64 * 1024;

const HYPA_ID: &str = "hypa";
const LOCAL_PLUGINS_ID: &str = "local-plugins";
const LOCAL_SETTINGS_ID: &str = "local-settings";

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum SectionKind {
    Hypa,
    LocalPlugins,
    LocalSettings,
}

impl SectionKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Hypa => HYPA_ID,
            Self::LocalPlugins => LOCAL_PLUGINS_ID,
            Self::LocalSettings => LOCAL_SETTINGS_ID,
        }
    }
    pub fn parse(id: &str) -> Result<Self> {
        match id {
            HYPA_ID => Ok(Self::Hypa),
            LOCAL_PLUGINS_ID => Ok(Self::LocalPlugins),
            LOCAL_SETTINGS_ID => Ok(Self::LocalSettings),
            _ => Err(FormatError("unknown-section")),
        }
    }
    /// Device-fixed data never reaches a synchronized state. Backup bundles
    /// accept it because restoring the same device is the point.
    pub fn is_synchronizable(self) -> bool {
        !matches!(self, Self::LocalSettings)
    }
    /// The seed that separates one section's content fingerprint from another's.
    pub fn fingerprint_domain(self) -> [u8; 32] {
        let mut bytes = b"risunest.section-fingerprint/v1\0".to_vec();
        bytes.extend_from_slice(self.id().as_bytes());
        hash(&bytes)
    }
}

/// Hypa keys are the normalized cache key. Plugin keys are an explicit
/// structure rather than a concatenation, so no owner, space or key can be
/// mistaken for another combination.
pub fn hypa_entry_key(cache_key: &str) -> Result<String> {
    if cache_key.len() != 64
        || !cache_key
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(FormatError("invalid-section-key"));
    }
    Ok(cache_key.into())
}

pub fn local_plugin_entry_key(owner: &str, space: &str, key: &str) -> Result<String> {
    if owner.is_empty() || space.is_empty() {
        return Err(FormatError("invalid-section-key"));
    }
    let encoded = serde_json::to_string(&[owner, space, key])
        .map_err(|_| FormatError("invalid-section-key"))?;
    if encoded.len() > 64 * 1024 {
        return Err(FormatError("invalid-section-key"));
    }
    Ok(encoded)
}

pub fn decode_local_plugin_entry_key(encoded: &str) -> Result<(String, String, String)> {
    let parts: Vec<String> =
        serde_json::from_str(encoded).map_err(|_| FormatError("invalid-section-key"))?;
    let [owner, space, key]: [String; 3] = parts
        .try_into()
        .map_err(|_| FormatError("invalid-section-key"))?;
    if local_plugin_entry_key(&owner, &space, &key)? != encoded {
        return Err(FormatError("invalid-section-key"));
    }
    Ok((owner, space, key))
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum PluginSpace {
    String,
    Json,
}

/// A vector rides inline while it is small and becomes its own object past the
/// inline limit. The reference is resolved through the same pack and object
/// path every other large value uses.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ObjectReference {
    pub content_sha256: [u8; 32],
    pub byte_length: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum InlineOrObject {
    #[serde(rename = "inline")]
    Inline(String),
    #[serde(rename = "object")]
    Object(ObjectReference),
}

impl InlineOrObject {
    pub fn inline(bytes: &[u8]) -> Result<Self> {
        use base64::Engine;
        if bytes.len() > MAX_INLINE_VALUE_BYTES {
            return Err(FormatError("section-value-requires-object"));
        }
        Ok(Self::Inline(
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        ))
    }
    pub fn decode_inline(&self) -> Result<Vec<u8>> {
        use base64::Engine;
        let Self::Inline(encoded) = self else {
            return Err(FormatError("section-value-is-an-object"));
        };
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| FormatError("invalid-section-value"))?;
        if bytes.len() > MAX_INLINE_VALUE_BYTES {
            return Err(FormatError("invalid-section-value"));
        }
        Ok(bytes)
    }
    fn validate(&self) -> Result<()> {
        match self {
            Self::Inline(_) => self.decode_inline().map(|_| ()),
            Self::Object(reference) => {
                if reference.byte_length == 0 || reference.byte_length > i64::MAX as u64 {
                    Err(FormatError("invalid-section-value"))
                } else {
                    Ok(())
                }
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HypaValue {
    pub producer: String,
    pub model: String,
    pub endpoint: Option<String>,
    pub preprocess_version: u32,
    pub dimensions: u32,
    pub vector: InlineOrObject,
    pub metadata: Option<serde_json::Value>,
}

/// Plugin values keep the storage API's own types. Arbitrary plugin JSON is
/// never rewritten by the normalization rules that apply to control metadata.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalPluginValue {
    pub space: PluginSpace,
    pub value: serde_json::Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LocalSettingValue {
    pub value: serde_json::Value,
}

/// A removal carries the commit it first reached a remote in and when that
/// happened, so every device judges its age the same way. The marker means
/// nothing outside the remote lineage that issued the commit number.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum SectionValue {
    #[serde(rename = "hypa")]
    Hypa(HypaValue),
    #[serde(rename = "localPlugin")]
    LocalPlugin(LocalPluginValue),
    #[serde(rename = "localSetting")]
    LocalSetting(LocalSettingValue),
    #[serde(rename = "tombstone")]
    Tombstone {
        first_published_generation: Sequence,
        first_published_at_ms: u64,
    },
}

impl SectionValue {
    pub fn kind(&self) -> Option<SectionKind> {
        match self {
            Self::Hypa(_) => Some(SectionKind::Hypa),
            Self::LocalPlugin(_) => Some(SectionKind::LocalPlugins),
            Self::LocalSetting(_) => Some(SectionKind::LocalSettings),
            Self::Tombstone { .. } => None,
        }
    }
    pub fn tombstone(first_published_generation: Sequence, first_published_at_ms: u64) -> Self {
        Self::Tombstone {
            first_published_generation,
            first_published_at_ms,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SectionEntryVersion {
    pub write_clock: Sequence,
    pub writer_id: String,
}

/// A backup bundle captures user values only, so `version` is absent there. A
/// synchronized section always carries one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SectionEntry {
    pub codec: String,
    pub kind: SectionKind,
    pub key: String,
    pub value: SectionValue,
    pub version: Option<SectionEntryVersion>,
}

impl SectionEntry {
    pub fn new(
        kind: SectionKind,
        key: String,
        value: SectionValue,
        version: Option<SectionEntryVersion>,
    ) -> Result<Self> {
        let entry = Self {
            codec: SECTION_CODEC.into(),
            kind,
            key,
            value,
            version,
        };
        entry.validate()?;
        Ok(entry)
    }
    pub fn validate(&self) -> Result<()> {
        if self.codec != SECTION_CODEC {
            return Err(FormatError("unsupported-section-codec"));
        }
        match self.kind {
            SectionKind::Hypa => hypa_entry_key(&self.key)?,
            SectionKind::LocalPlugins => {
                decode_local_plugin_entry_key(&self.key)?;
                self.key.clone()
            }
            SectionKind::LocalSettings => {
                if self.key.is_empty() || self.key.len() > 64 * 1024 {
                    return Err(FormatError("invalid-section-key"));
                }
                self.key.clone()
            }
        };
        if self
            .value
            .kind()
            .is_some_and(|kind| kind != self.kind)
        {
            return Err(FormatError("section-value-kind-mismatch"));
        }
        match &self.value {
            SectionValue::Hypa(value) => {
                if value.producer.is_empty()
                    || value.model.is_empty()
                    || value.dimensions == 0
                    || value
                        .endpoint
                        .as_ref()
                        .is_some_and(|endpoint| endpoint.is_empty())
                {
                    return Err(FormatError("invalid-section-value"));
                }
                value.vector.validate()?;
                let byte_length = match &value.vector {
                    InlineOrObject::Inline(_) => value.vector.decode_inline()?.len() as u64,
                    InlineOrObject::Object(reference) => reference.byte_length,
                };
                if byte_length != u64::from(value.dimensions) * 4 {
                    return Err(FormatError("invalid-section-vector-length"));
                }
            }
            SectionValue::LocalPlugin(value) => {
                let (_, space, _) = decode_local_plugin_entry_key(&self.key)?;
                let declared = match value.space {
                    PluginSpace::String => "string",
                    PluginSpace::Json => "json",
                };
                if space != declared {
                    return Err(FormatError("section-plugin-space-mismatch"));
                }
                if matches!(value.space, PluginSpace::String) && !value.value.is_string() {
                    return Err(FormatError("invalid-section-value"));
                }
            }
            // A removal is only ever published as part of a commit, so a
            // marker naming no commit or no time is not a removal this format
            // can carry.
            SectionValue::Tombstone {
                first_published_generation,
                first_published_at_ms,
            } => {
                if *first_published_generation == Sequence::from(0u64)
                    || *first_published_at_ms == 0
                    || *first_published_at_ms > i64::MAX as u64
                {
                    return Err(FormatError("invalid-section-value"));
                }
            }
            SectionValue::LocalSetting(_) => {}
        }
        if self
            .version
            .as_ref()
            .is_some_and(|version| version.writer_id.is_empty() || version.writer_id.len() > 1024)
        {
            return Err(FormatError("invalid-section-version"));
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| FormatError("invalid-section-entry"))?;
        if bytes.len() > MAX_SECTION_ENTRY_BYTES {
            return Err(FormatError("section-entry-limit-exceeded"));
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.is_empty() || bytes.len() > MAX_SECTION_ENTRY_BYTES {
            return Err(FormatError("section-entry-limit-exceeded"));
        }
        let entry: Self =
            serde_json::from_slice(bytes).map_err(|_| FormatError("invalid-section-entry"))?;
        entry.validate()?;
        if entry.encode()? != bytes {
            return Err(FormatError("non-canonical-section-entry"));
        }
        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn version(clock: u64) -> Option<SectionEntryVersion> {
        Some(SectionEntryVersion {
            write_clock: Sequence::from(clock),
            writer_id: "writer-a".into(),
        })
    }

    fn hypa_entry(cache_key: &str) -> SectionEntry {
        SectionEntry::new(
            SectionKind::Hypa,
            hypa_entry_key(cache_key).unwrap(),
            SectionValue::Hypa(HypaValue {
                producer: "hypa-v2".into(),
                model: "text-embedding".into(),
                endpoint: None,
                preprocess_version: 1,
                dimensions: 4,
                vector: InlineOrObject::inline(&[0u8; 16]).unwrap(),
                metadata: None,
            }),
            version(7),
        )
        .unwrap()
    }

    #[test]
    fn section_ids_keys_counters_and_empty_sections_round_trip() {
        let cache_key = "a".repeat(64);
        let entries = [
            hypa_entry(&cache_key),
            SectionEntry::new(
                SectionKind::LocalPlugins,
                local_plugin_entry_key("provider-manager", "json", "settings").unwrap(),
                SectionValue::LocalPlugin(LocalPluginValue {
                    space: PluginSpace::Json,
                    value: serde_json::json!({ "b": 1, "a": [2, 3] }),
                }),
                version(9),
            )
            .unwrap(),
            SectionEntry::new(
                SectionKind::LocalPlugins,
                local_plugin_entry_key("yumi-translator", "string", "token").unwrap(),
                SectionValue::LocalPlugin(LocalPluginValue {
                    space: PluginSpace::String,
                    value: serde_json::Value::String("kept".into()),
                }),
                None,
            )
            .unwrap(),
            SectionEntry::new(
                SectionKind::Hypa,
                hypa_entry_key(&"b".repeat(64)).unwrap(),
                SectionValue::tombstone(Sequence::from(3u64), 1_760_000_000_000),
                version(10),
            )
            .unwrap(),
        ];
        for entry in &entries {
            let encoded = entry.encode().unwrap();
            assert_eq!(&SectionEntry::decode(&encoded).unwrap(), entry);
        }
        // An empty section is a real, publishable state. It differs from a
        // section that was never published, which carries no reference at all.
        let empty: BTreeMap<String, [u8; 32]> = BTreeMap::new();
        assert_ne!(
            crate::format::fingerprint(&SectionKind::Hypa.fingerprint_domain(), &empty),
            crate::format::fingerprint(&SectionKind::LocalPlugins.fingerprint_domain(), &empty)
        );
    }

    #[test]
    fn plugin_keys_are_structured_rather_than_concatenated() {
        let ambiguous = local_plugin_entry_key("a", "b", "c:d").unwrap();
        let other = local_plugin_entry_key("a", "b:c", "d").unwrap();
        assert_ne!(ambiguous, other);
        assert_eq!(
            decode_local_plugin_entry_key(&ambiguous).unwrap(),
            ("a".into(), "b".into(), "c:d".into())
        );
        assert!(decode_local_plugin_entry_key("[\"a\",\"b\"]").is_err());
        assert!(decode_local_plugin_entry_key("[\"a\", \"b\", \"c\"]").is_err());
        assert!(hypa_entry_key("A".repeat(64).as_str()).is_err());
        assert!(hypa_entry_key("abc").is_err());
    }

    #[test]
    fn arbitrary_plugin_json_keeps_its_own_shape_and_typed_spaces_are_enforced() {
        let value = serde_json::json!({ "zeta": 1, "alpha": { "nested": [1, 2] } });
        let entry = SectionEntry::new(
            SectionKind::LocalPlugins,
            local_plugin_entry_key("owner", "json", "key").unwrap(),
            SectionValue::LocalPlugin(LocalPluginValue {
                space: PluginSpace::Json,
                value: value.clone(),
            }),
            None,
        )
        .unwrap();
        let decoded = SectionEntry::decode(&entry.encode().unwrap()).unwrap();
        let SectionValue::LocalPlugin(restored) = decoded.value else {
            panic!("plugin value changed kind");
        };
        assert_eq!(restored.value, value);
        assert_eq!(
            serde_json::to_string(&restored.value).unwrap(),
            serde_json::to_string(&value).unwrap()
        );
        assert!(SectionEntry::new(
            SectionKind::LocalPlugins,
            local_plugin_entry_key("owner", "string", "key").unwrap(),
            SectionValue::LocalPlugin(LocalPluginValue {
                space: PluginSpace::String,
                value: serde_json::json!(5),
            }),
            None,
        )
        .is_err());
    }

    #[test]
    fn a_value_over_the_inline_limit_becomes_an_object_reference() {
        assert!(InlineOrObject::inline(&vec![0u8; MAX_INLINE_VALUE_BYTES]).is_ok());
        assert_eq!(
            InlineOrObject::inline(&vec![0u8; MAX_INLINE_VALUE_BYTES + 1]),
            Err(FormatError("section-value-requires-object"))
        );
        let reference = InlineOrObject::Object(ObjectReference {
            content_sha256: [3; 32],
            byte_length: 1536 * 4,
        });
        assert!(reference.decode_inline().is_err());
        let entry = SectionEntry::new(
            SectionKind::Hypa,
            hypa_entry_key(&"c".repeat(64)).unwrap(),
            SectionValue::Hypa(HypaValue {
                producer: "hypa-v3-group".into(),
                model: "text-embedding".into(),
                endpoint: Some("https://synthetic.invalid/embed".into()),
                preprocess_version: 1,
                dimensions: 1536,
                vector: reference,
                metadata: Some(serde_json::json!({ "chunk": 2 })),
            }),
            version(11),
        )
        .unwrap();
        assert_eq!(
            SectionEntry::decode(&entry.encode().unwrap()).unwrap(),
            entry
        );
    }

    #[test]
    fn vector_lengths_plugin_spaces_and_removal_times_are_checked_before_apply() {
        let mut entry = hypa_entry(&"a".repeat(64));
        let SectionValue::Hypa(value) = &mut entry.value else { unreachable!() };
        value.dimensions += 1;
        assert_eq!(entry.validate(), Err(FormatError("invalid-section-vector-length")));
        let SectionValue::Hypa(value) = &mut entry.value else { unreachable!() };
        value.vector = InlineOrObject::Object(ObjectReference { content_sha256: [1; 32], byte_length: 16 });
        assert_eq!(entry.validate(), Err(FormatError("invalid-section-vector-length")));

        assert_eq!(SectionEntry::new(
            SectionKind::LocalPlugins,
            local_plugin_entry_key("owner", "json", "key").unwrap(),
            SectionValue::LocalPlugin(LocalPluginValue {
                space: PluginSpace::String, value: serde_json::Value::String("value".into()),
            }),
            version(1),
        ), Err(FormatError("section-plugin-space-mismatch")));
        assert_eq!(SectionEntry::new(
            SectionKind::Hypa, hypa_entry_key(&"a".repeat(64)).unwrap(),
            SectionValue::tombstone(super::Sequence::from(1u64), i64::MAX as u64 + 1), version(1),
        ), Err(FormatError("invalid-section-value")));
    }

    #[test]
    fn an_unknown_section_or_codec_is_reported_rather_than_narrowed() {
        assert_eq!(SectionKind::parse("device"), Err(FormatError("unknown-section")));
        assert_eq!(SectionKind::parse(""), Err(FormatError("unknown-section")));
        assert!(SectionKind::parse("hypa").unwrap().is_synchronizable());
        assert!(!SectionKind::parse("local-settings")
            .unwrap()
            .is_synchronizable());
        let mut entry = hypa_entry(&"d".repeat(64));
        entry.codec = "risunest.section-codec/v0".into();
        assert_eq!(entry.validate(), Err(FormatError("unsupported-section-codec")));
        let smuggled = serde_json::json!({
            "codec": SECTION_CODEC,
            "kind": "hypa",
            "key": "e".repeat(64),
            "value": { "tombstone": {
                "firstPublishedGeneration": "3",
                "firstPublishedAtMs": 1_760_000_000_000u64,
            } },
            "version": null,
            "extra": 1,
        });
        assert!(SectionEntry::decode(&serde_json::to_vec(&smuggled).unwrap()).is_err());
    }

    /// A removal always names the commit it first reached a remote in. A
    /// marker naming none is not a shape this format accepts, in either
    /// direction.
    #[test]
    fn a_removal_carries_the_commit_it_was_first_published_in() {
        let entry = SectionEntry::new(
            SectionKind::Hypa,
            hypa_entry_key(&"1".repeat(64)).unwrap(),
            SectionValue::tombstone(Sequence::from(12u64), 1_760_000_000_000),
            version(10),
        )
        .unwrap();
        let encoded = entry.encode().unwrap();
        assert!(String::from_utf8_lossy(&encoded).contains(
            "\"value\":{\"tombstone\":{\"firstPublishedGeneration\":\"12\",\"firstPublishedAtMs\":1760000000000}}"
        ));
        assert_eq!(SectionEntry::decode(&encoded).unwrap(), entry);
        assert_eq!(
            SectionEntry::new(
                SectionKind::Hypa,
                hypa_entry_key(&"2".repeat(64)).unwrap(),
                SectionValue::tombstone(Sequence::from(0u64), 1_760_000_000_000),
                version(10),
            ),
            Err(FormatError("invalid-section-value"))
        );
        assert_eq!(
            SectionEntry::new(
                SectionKind::Hypa,
                hypa_entry_key(&"3".repeat(64)).unwrap(),
                SectionValue::tombstone(Sequence::from(12u64), 0),
                version(10),
            ),
            Err(FormatError("invalid-section-value"))
        );
        let unmarked = serde_json::json!({
            "codec": SECTION_CODEC,
            "kind": "hypa",
            "key": "4".repeat(64),
            "value": { "tombstone": {} },
            "version": null,
        });
        assert!(SectionEntry::decode(&serde_json::to_vec(&unmarked).unwrap()).is_err());
    }

    #[test]
    fn a_value_cannot_claim_a_different_section_than_its_entry() {
        assert_eq!(
            SectionEntry::new(
                SectionKind::Hypa,
                hypa_entry_key(&"f".repeat(64)).unwrap(),
                SectionValue::LocalPlugin(LocalPluginValue {
                    space: PluginSpace::Json,
                    value: serde_json::json!(1),
                }),
                None,
            ),
            Err(FormatError("section-value-kind-mismatch"))
        );
    }
}
