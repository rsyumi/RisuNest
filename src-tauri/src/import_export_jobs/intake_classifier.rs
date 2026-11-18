//! Import intake classification: what a dropped, opened or shared file holds,
//! read where it lies. Content decides first and the display name's extension
//! second; the importer that follows still validates the whole file.
use super::classifier::probe_charx;
use super::FormatErrorKind;
use crate::native_file_jobs::charx::CharXLimits;
use crate::native_file_jobs::{
    detect, open_job_source, BackupSourceFormat, JobSource, NativeFileJobState,
};
use serde::de::{self, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use tauri::State;

const PREFIX_BYTES: u64 = 64;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
/// Far above real cards, which spend one chunk per embedded asset.
const PNG_MAX_CHUNKS: usize = 1 << 20;
const PNG_MAX_CHUNK_BYTES: u64 = i32::MAX as u64;
/// A `tEXt` keyword is 1 to 79 bytes followed by its NUL separator.
const PNG_KEYWORD_AND_SEPARATOR_BYTES: usize = 80;
/// Small enough that skipping a large chunk does not read it.
const PNG_WALK_BUFFER_BYTES: usize = 256;
const ZIP_LOCAL_ENTRY: &[u8] = b"PK\x03\x04";
const ZIP_LOCAL_HEADER_BYTES: usize = 30;
const ZIP64_EXTRA_ID: u16 = 0x0001;
const PORTABLE_FORMAT_ENTRY: &[u8] = b"format.json";
const PORTABLE_FORMAT_MAX_BYTES: u64 = 16 * 1024;
const JSON_READ_BUFFER_BYTES: usize = 64 * 1024;
/// The JSON importers stop at this size, and the classifier would hold a long string whole.
const JSON_MAX_BYTES: u64 = super::MAX_CONTENT_METADATA_BYTES as u64;
const UNCLASSIFIED_SOURCE_MESSAGE: &str = "this source cannot be classified in place";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportIntakeClassifyRequest {
    sources: Vec<ImportIntakeClassifySource>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportIntakeClassifySource {
    source: JobSource,
    display_name: String,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IntakeKind {
    Character,
    Module,
    Lorebook,
    Preset,
    Backup,
    Script,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IntakeFormat {
    Png,
    Charx,
    JpegCharx,
    Json,
    Risum,
    Risup,
    Risupreset,
    Portable,
    BlockRisuSave,
    LocalBackup,
    Js,
    Ts,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IntakeRefusal {
    UnsupportedFormat,
    PlainJpeg,
    PngWithoutCard,
    ReadFailed,
    TooLarge,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ImportIntakeClassification {
    kind: IntakeKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    format: Option<IntakeFormat>,
    #[serde(skip_serializing_if = "Option::is_none")]
    refusal: Option<IntakeRefusal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Found {
    Content(IntakeKind, IntakeFormat),
    Refused(IntakeRefusal),
}

/// Classifies each source in request order. A source that cannot be read is
/// refused on its own; the call fails only when the request does not parse.
#[tauri::command(async)]
pub(crate) fn import_intake_classify(
    state: State<'_, NativeFileJobState>,
    request: ImportIntakeClassifyRequest,
) -> Vec<ImportIntakeClassification> {
    classify_sources(state.job_root(), &request.sources)
}

fn classify_sources(
    job_root: &Path,
    sources: &[ImportIntakeClassifySource],
) -> Vec<ImportIntakeClassification> {
    sources
        .iter()
        .map(|item| classify_source(job_root, &item.source, &item.display_name))
        .collect()
}

fn classify_source(
    job_root: &Path,
    source: &JobSource,
    display_name: &str,
) -> ImportIntakeClassification {
    match source {
        // Opening an Android spool only resolves it, so a later import can still claim it.
        JobSource::DesktopPath { .. } | JobSource::AndroidSpool { .. } => {}
        JobSource::AndroidSeekable { .. }
        | JobSource::IosScoped { .. }
        | JobSource::SavedConflict { .. } => {
            return read_failed(None, UNCLASSIFIED_SOURCE_MESSAGE.to_owned())
        }
    }
    let mut opened = match open_job_source(job_root, source) {
        Ok(opened) => opened,
        Err(error) => return read_failed(None, error.message),
    };
    let bytes = Some(opened.total_bytes);
    let found = classify_file(
        &mut opened.file,
        opened.total_bytes,
        &extension(display_name),
    );
    match found {
        Ok(Found::Content(kind, format)) => ImportIntakeClassification {
            kind,
            format: Some(format),
            refusal: None,
            message: None,
            bytes,
        },
        Ok(Found::Refused(refusal)) => ImportIntakeClassification {
            kind: IntakeKind::Unsupported,
            format: None,
            refusal: Some(refusal),
            message: None,
            bytes,
        },
        Err(error) => read_failed(bytes, error.to_string()),
    }
}

fn read_failed(bytes: Option<u64>, message: String) -> ImportIntakeClassification {
    ImportIntakeClassification {
        kind: IntakeKind::Unsupported,
        format: None,
        refusal: Some(IntakeRefusal::ReadFailed),
        message: Some(message),
        bytes,
    }
}

/// The lowercased text after the name's last dot, as the TypeScript importers read it.
fn extension(display_name: &str) -> String {
    let name = display_name
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(display_name);
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_lowercase())
        .unwrap_or_default()
}

fn classify_file(
    file: &mut File,
    file_length: u64,
    extension: &str,
) -> io::Result<Found> {
    use IntakeFormat as Format;
    use IntakeKind as Kind;

    file.seek(SeekFrom::Start(0))?;
    let mut prefix = Vec::with_capacity(PREFIX_BYTES as usize);
    file.by_ref().take(PREFIX_BYTES).read_to_end(&mut prefix)?;

    // `detect` falls back to any zlib header, so it only sees backup signatures and backup names.
    // A ZIP reaches it only through a portable `format.json` first entry: the streaming reader
    // drains an entry it rejects.
    let backup_candidate = if prefix.starts_with(ZIP_LOCAL_ENTRY) {
        is_portable_format_entry(file, &prefix)?
    } else {
        is_risusave_magic(&prefix) || matches!(extension, "risunest" | "risudat" | "bin")
    };
    if backup_candidate {
        file.seek(SeekFrom::Start(0))?;
        if let Ok(format) = detect(file.try_clone()?) {
            return Ok(Found::Content(Kind::Backup, backup_format(format)));
        }
    }
    if prefix.starts_with(&[0xff, 0xd8, 0xff]) {
        return Ok(if has_charx_card(file)? {
            Found::Content(Kind::Character, Format::JpegCharx)
        } else {
            Found::Refused(IntakeRefusal::PlainJpeg)
        });
    }
    if prefix.starts_with(PNG_SIGNATURE) {
        return Ok(if png_has_card(file, file_length, PNG_MAX_CHUNKS)? {
            Found::Content(Kind::Character, Format::Png)
        } else {
            Found::Refused(IntakeRefusal::PngWithoutCard)
        });
    }
    if prefix.starts_with(&[0x6f, 0x00]) {
        return Ok(Found::Content(Kind::Module, Format::Risum));
    }
    if has_charx_card(file)? {
        return Ok(Found::Content(Kind::Character, Format::Charx));
    }
    if let Some(found) = classify_json(file, file_length)? {
        return Ok(found);
    }
    Ok(match extension {
        "risup" => Found::Content(Kind::Preset, Format::Risup),
        "risupreset" => Found::Content(Kind::Preset, Format::Risupreset),
        "js" => Found::Content(Kind::Script, Format::Js),
        "ts" => Found::Content(Kind::Script, Format::Ts),
        _ => Found::Refused(IntakeRefusal::UnsupportedFormat),
    })
}

fn backup_format(format: BackupSourceFormat) -> IntakeFormat {
    match format {
        BackupSourceFormat::Portable => IntakeFormat::Portable,
        BackupSourceFormat::BlockRisuSave => IntakeFormat::BlockRisuSave,
        BackupSourceFormat::LocalBackup => IntakeFormat::LocalBackup,
    }
}

/// The RisuSave magics `detect` accepts.
fn is_risusave_magic(prefix: &[u8]) -> bool {
    prefix.starts_with(b"RISUSAVE\0")
        || prefix.starts_with(b"\0RISUSAVE\0")
        || prefix.starts_with(b"\0\0RISU")
}

/// The first local ZIP entry is a `format.json` small enough for `detect` to accept. The
/// portable writer marks its sizes for ZIP64, so they may sit in the entry's extra field.
fn is_portable_format_entry(file: &mut File, prefix: &[u8]) -> io::Result<bool> {
    let name_end = ZIP_LOCAL_HEADER_BYTES + PORTABLE_FORMAT_ENTRY.len();
    if le_u16(prefix, 26) != Some(PORTABLE_FORMAT_ENTRY.len() as u16)
        || prefix.get(ZIP_LOCAL_HEADER_BYTES..name_end) != Some(PORTABLE_FORMAT_ENTRY)
    {
        return Ok(false);
    }
    let (Some(compressed), Some(uncompressed), Some(extra_length)) =
        (le_u32(prefix, 18), le_u32(prefix, 22), le_u16(prefix, 28))
    else {
        return Ok(false);
    };
    let compressed = if compressed == u32::MAX {
        let mut extra = vec![0_u8; usize::from(extra_length)];
        file.seek(SeekFrom::Start(name_end as u64))?;
        if !read_full(file, &mut extra)? {
            return Ok(false);
        }
        match zip64_compressed_size(&extra, uncompressed == u32::MAX) {
            Some(size) => size,
            None => return Ok(false),
        }
    } else {
        u64::from(compressed)
    };
    Ok(compressed <= PORTABLE_FORMAT_MAX_BYTES)
}

/// The compressed size in a local ZIP64 extra field, which lists the uncompressed
/// size first when that one is marked too.
fn zip64_compressed_size(mut extra: &[u8], uncompressed_marked: bool) -> Option<u64> {
    while let (Some(id), Some(length)) = (le_u16(extra, 0), le_u16(extra, 2)) {
        let end = 4 + usize::from(length);
        let data = extra.get(4..end)?;
        if id == ZIP64_EXTRA_ID {
            let offset = if uncompressed_marked { 8 } else { 0 };
            return data.get(offset..offset + 8).map(|bytes| {
                u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ])
            });
        }
        extra = &extra[end..];
    }
    None
}

fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset + 2)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset + 4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// The CharX probe of the content import, with its classification limits.
fn has_charx_card(reader: &mut (impl Read + Seek)) -> io::Result<bool> {
    let limits = CharXLimits::default();
    match probe_charx(
        reader,
        limits.max_metadata_bytes,
        limits.max_entries,
        limits.max_directory_bytes,
        &|| false,
    ) {
        Ok(found) => Ok(found),
        Err(error) if error.kind == FormatErrorKind::Io => Err(io::Error::other(error)),
        Err(_) => Ok(false),
    }
}

/// Walks chunk headers until `IEND` and reads only `tEXt` keywords. A chunk
/// that runs past the end of the file, or more than `max_chunks` chunks, ends the walk.
fn png_has_card(
    reader: &mut (impl Read + Seek),
    file_length: u64,
    max_chunks: usize,
) -> io::Result<bool> {
    let mut position = PNG_SIGNATURE.len() as u64;
    reader.seek(SeekFrom::Start(position))?;
    let mut reader = BufReader::with_capacity(PNG_WALK_BUFFER_BYTES, reader);
    for _ in 0..max_chunks {
        let mut header = [0_u8; 8];
        if !read_full(&mut reader, &mut header)? {
            return Ok(false);
        }
        let length = u64::from(u32::from_be_bytes([header[0], header[1], header[2], header[3]]));
        let Some(end) = position
            .checked_add(12 + length)
            .filter(|end| length <= PNG_MAX_CHUNK_BYTES && *end <= file_length)
        else {
            return Ok(false);
        };
        let mut skip = length + 4;
        match &header[4..] {
            b"IEND" => return Ok(false),
            b"tEXt" => {
                let mut keyword = [0_u8; PNG_KEYWORD_AND_SEPARATOR_BYTES];
                let read = length.min(keyword.len() as u64) as usize;
                reader.read_exact(&mut keyword[..read])?;
                let separator = keyword[..read].iter().position(|byte| *byte == 0);
                if separator.is_some_and(|separator| matches!(&keyword[..separator], b"ccv3" | b"chara")) {
                    return Ok(true);
                }
                skip -= read as u64;
            }
            _ => {}
        }
        reader.seek_relative(skip as i64)?;
        position = end;
    }
    Ok(false)
}

/// Fills `output`, or reports false when the input ends first.
fn read_full(reader: &mut impl Read, output: &mut [u8]) -> io::Result<bool> {
    let mut filled = 0;
    while filled < output.len() {
        match reader.read(&mut output[filled..]) {
            Ok(0) => return Ok(false),
            Ok(read) => filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(true)
}

/// Classifies a JSON object by its top-level keys; `None` when the file does not open an object.
/// A byte order mark is not skipped because the importers reject it.
fn classify_json(file: &mut File, file_length: u64) -> io::Result<Option<Found>> {
    file.seek(SeekFrom::Start(0))?;
    let mut reader = BufReader::with_capacity(JSON_READ_BUFFER_BYTES, file);
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(None);
        }
        match available
            .iter()
            .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r'))
        {
            Some(index) => {
                let opens_object = available[index] == b'{';
                reader.consume(index);
                if !opens_object {
                    return Ok(None);
                }
                if file_length > JSON_MAX_BYTES {
                    return Ok(Some(Found::Refused(IntakeRefusal::TooLarge)));
                }
                break;
            }
            None => {
                let length = available.len();
                reader.consume(length);
            }
        }
    }
    let mut deserializer = serde_json::Deserializer::from_reader(reader);
    let facts = Deserializer::deserialize_map(&mut deserializer, TopLevelVisitor)
        .and_then(|facts| deserializer.end().map(|()| facts));
    match facts {
        Ok(facts) => Ok(Some(facts.classify())),
        Err(error) if error.is_io() => Err(io::Error::from(error)),
        Err(_) => Ok(Some(Found::Refused(IntakeRefusal::UnsupportedFormat))),
    }
}

/// The last value of one top-level key, reduced to what the import rules test.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Shape {
    #[default]
    Absent,
    Null,
    Bool(bool),
    Number(f64),
    String { empty: bool, tag: Option<Tag> },
    Array,
    Object,
}

impl Shape {
    /// JavaScript truthiness.
    fn truthy(self) -> bool {
        match self {
            Shape::Absent | Shape::Null => false,
            Shape::Bool(value) => value,
            Shape::Number(value) => value != 0.0 && !value.is_nan(),
            Shape::String { empty, .. } => !empty,
            Shape::Array | Shape::Object => true,
        }
    }

    fn tag(self) -> Option<Tag> {
        match self {
            Shape::String { tag, .. } => tag,
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tag {
    CharacterCardSpec,
    RisuModule,
    Risu,
    Regex,
}

impl Tag {
    fn of(value: &str) -> Option<Self> {
        match value {
            "chara_card_v2" | "chara_card_v3" => Some(Tag::CharacterCardSpec),
            "risuModule" => Some(Tag::RisuModule),
            "risu" => Some(Tag::Risu),
            "regex" => Some(Tag::Regex),
            _ => None,
        }
    }
}

#[derive(Default)]
struct JsonFacts {
    spec: Shape,
    kind: Shape,
    data: Shape,
    entries: Shape,
    char_name: Shape,
    name: Shape,
    char_persona: Shape,
    description: Shape,
    char_greeting: Shape,
    first_mes: Shape,
    preset_version: Shape,
    parameters: Shape,
    prompt_order: Shape,
    prompts: Shape,
    risu_preset_key: bool,
}

impl JsonFacts {
    /// In order: `importCharacterCardSpec`, `importModuleData`, the off-spec
    /// card of `importCharacterProcess`, then the NovelAI, SillyTavern and
    /// RisuAI branches of `importPreset`.
    fn classify(&self) -> Found {
        let kind = if self.spec.tag() == Some(Tag::CharacterCardSpec) {
            Some(IntakeKind::Character)
        } else if self.kind.tag() == Some(Tag::RisuModule) {
            Some(IntakeKind::Module)
        } else if self.kind.tag() == Some(Tag::Risu) && self.data == Shape::Array {
            Some(IntakeKind::Lorebook)
        } else if self.entries.truthy() {
            Some(IntakeKind::Lorebook)
        } else if self.kind.tag() == Some(Tag::Regex) && self.data.truthy() {
            Some(IntakeKind::Module)
        } else if (self.char_name.truthy() || self.name.truthy())
            && (self.char_persona.truthy() || self.description.truthy())
            && (self.char_greeting.truthy() || self.first_mes.truthy())
        {
            Some(IntakeKind::Character)
        } else if matches!(self.preset_version, Shape::Number(version) if version >= 3.0)
            && self.parameters == Shape::Object
        {
            Some(IntakeKind::Preset)
        } else if self.prompt_order == Shape::Array && self.prompts == Shape::Array {
            Some(IntakeKind::Preset)
        } else if self.risu_preset_key {
            Some(IntakeKind::Preset)
        } else {
            None
        };
        kind.map_or(Found::Refused(IntakeRefusal::UnsupportedFormat), |kind| {
            Found::Content(kind, IntakeFormat::Json)
        })
    }
}

enum Key {
    Spec,
    Type,
    Data,
    Entries,
    CharName,
    Name,
    CharPersona,
    Description,
    CharGreeting,
    FirstMes,
    PresetVersion,
    Parameters,
    PromptOrder,
    Prompts,
    RisuPreset,
    Other,
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_identifier(KeyVisitor)
    }
}

struct KeyVisitor;

impl Visitor<'_> for KeyVisitor {
    type Value = Key;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("an object key")
    }

    fn visit_str<E: de::Error>(self, key: &str) -> Result<Key, E> {
        Ok(match key {
            "spec" => Key::Spec,
            "type" => Key::Type,
            "data" => Key::Data,
            "entries" => Key::Entries,
            "char_name" => Key::CharName,
            "name" => Key::Name,
            "char_persona" => Key::CharPersona,
            "description" => Key::Description,
            "char_greeting" => Key::CharGreeting,
            "first_mes" => Key::FirstMes,
            "presetVersion" => Key::PresetVersion,
            "parameters" => Key::Parameters,
            "prompt_order" => Key::PromptOrder,
            "prompts" => Key::Prompts,
            "mainPrompt" | "jailbreak" | "globalNote" | "formatingOrder" | "promptTemplate"
            | "aiModel" | "subModel" => Key::RisuPreset,
            _ => Key::Other,
        })
    }
}

impl<'de> Deserialize<'de> for Shape {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ShapeVisitor)
    }
}

/// Reads one value without keeping it: containers are drained unread.
struct ShapeVisitor;

impl<'de> Visitor<'de> for ShapeVisitor {
    type Value = Shape;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Shape, E> {
        Ok(Shape::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Shape, E> {
        Ok(Shape::Number(value as f64))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Shape, E> {
        Ok(Shape::Number(value as f64))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Shape, E> {
        Ok(Shape::Number(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Shape, E> {
        Ok(Shape::String {
            empty: value.is_empty(),
            tag: Tag::of(value),
        })
    }

    fn visit_unit<E: de::Error>(self) -> Result<Shape, E> {
        Ok(Shape::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> Result<Shape, A::Error> {
        while sequence.next_element::<IgnoredAny>()?.is_some() {}
        Ok(Shape::Array)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Shape, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(Shape::Object)
    }
}

/// Accepts only an object; a repeated key keeps its last value, as `JSON.parse` does.
struct TopLevelVisitor;

impl<'de> Visitor<'de> for TopLevelVisitor {
    type Value = JsonFacts;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<JsonFacts, A::Error> {
        let mut facts = JsonFacts::default();
        while let Some(key) = map.next_key::<Key>()? {
            let slot = match key {
                Key::Spec => &mut facts.spec,
                Key::Type => &mut facts.kind,
                Key::Data => &mut facts.data,
                Key::Entries => &mut facts.entries,
                Key::CharName => &mut facts.char_name,
                Key::Name => &mut facts.name,
                Key::CharPersona => &mut facts.char_persona,
                Key::Description => &mut facts.description,
                Key::CharGreeting => &mut facts.char_greeting,
                Key::FirstMes => &mut facts.first_mes,
                Key::PresetVersion => &mut facts.preset_version,
                Key::Parameters => &mut facts.parameters,
                Key::PromptOrder => &mut facts.prompt_order,
                Key::Prompts => &mut facts.prompts,
                Key::RisuPreset => {
                    facts.risu_preset_key = true;
                    map.next_value::<IgnoredAny>()?;
                    continue;
                }
                Key::Other => {
                    map.next_value::<IgnoredAny>()?;
                    continue;
                }
            };
            *slot = map.next_value::<Shape>()?;
        }
        Ok(facts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_file_jobs::claim_spool_source_for_test;
    use serde_json::{json, Value};
    use std::fs;
    use std::io::{Cursor, Write};
    use uuid::Uuid;

    const JPEG: &[u8] = &[
        0xff, 0xd8, 0xff, 0xe0, 0, 16, b'J', b'F', b'I', b'F', 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 0xff,
        0xd9,
    ];
    /// The first bytes of every `.risup`: the RPack encoding of a gzip header.
    const RISUP: &[u8] = &[0x10, 0x03, 0xfc, 0x41, 0x52, 0x63, 0x07, 0x99];
    const CARD_TEXT: &str = "eyJzcGVjIjoiY2hhcmFfY2FyZF92MiJ9";

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut crc = crc32fast::Hasher::new();
        crc.update(kind);
        crc.update(data);
        [
            &(data.len() as u32).to_be_bytes()[..],
            kind,
            data,
            &crc.finalize().to_be_bytes(),
        ]
        .concat()
    }

    fn text(keyword: &str, value: &str) -> Vec<u8> {
        chunk(b"tEXt", &[keyword.as_bytes(), b"\0", value.as_bytes()].concat())
    }

    fn png(chunks: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
        let mut bytes = PNG_SIGNATURE.to_vec();
        chunks.into_iter().for_each(|chunk| bytes.extend(chunk));
        bytes
    }

    /// A one-pixel image with `extra` chunks between its data and `IEND`.
    fn image(extra: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
        let header = chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        let data = chunk(b"IDAT", &[1, 2, 3, 4]);
        png([header, data]
            .into_iter()
            .chain(extra)
            .chain([chunk(b"IEND", &[])]))
    }

    fn zip(entries: &[(&str, &[u8])], method: zip::CompressionMethod) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, bytes) in entries {
            let options = zip::write::FileOptions::default().compression_method(method);
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn charx() -> Vec<u8> {
        zip(
            &[
                ("assets/icon/main.png", &image([])),
                ("card.json", br#"{"spec":"chara_card_v3","data":{"name":"Synthetic"}}"#),
            ],
            zip::CompressionMethod::Deflated,
        )
    }

    fn portable() -> Vec<u8> {
        zip(
            &[
                ("format.json", br#"{"magic":"risunest-portable-backup","version":1}"#),
                ("database.json", b"{}"),
            ],
            zip::CompressionMethod::Stored,
        )
    }

    /// An empty backup from the product writer, which stores ZIP64 sizes in each local header.
    fn product_portable() -> Vec<u8> {
        struct Never;
        impl crate::local_backup::CancellationProbe for Never {
            fn is_cancelled(&self) -> bool {
                false
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let jobs = directory.path().join("jobs");
        fs::create_dir(&jobs).unwrap();
        let path = directory.path().join("backup.risunest");
        crate::portable_backup::Catalog::create(&jobs, "synthetic-intake", 0)
            .unwrap()
            .write_candidate(&path, false, &Never)
            .unwrap();
        fs::read(path).unwrap()
    }

    fn gzip(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn zlib(bytes: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn content(kind: &str, format: &str) -> Value {
        json!({ "kind": kind, "format": format })
    }

    fn refused(refusal: &str) -> Value {
        json!({ "kind": "unsupported", "refusal": refusal })
    }

    fn with_bytes(mut expected: Value, bytes: usize) -> Value {
        expected["bytes"] = json!(bytes);
        expected
    }

    /// Runs the command body on the request as the TypeScript wrapper sends it.
    fn respond(job_root: &Path, sources: Vec<Value>) -> Vec<Value> {
        let request: ImportIntakeClassifyRequest =
            serde_json::from_value(json!({ "sources": sources })).unwrap();
        match serde_json::to_value(classify_sources(job_root, &request.sources)).unwrap() {
            Value::Array(items) => items,
            other => panic!("response is not an array: {other}"),
        }
    }

    fn desktop_sources(root: &Path, files: &[(&str, Vec<u8>)]) -> Vec<Value> {
        files
            .iter()
            .enumerate()
            .map(|(index, (name, bytes))| {
                let directory = root.join("files").join(index.to_string());
                fs::create_dir_all(&directory).unwrap();
                let path = directory.join(name);
                fs::write(&path, bytes).unwrap();
                json!({ "source": { "type": "desktopPath", "path": path }, "displayName": name })
            })
            .collect()
    }

    fn write_spool(job_root: &Path, display_name: &str, bytes: &[u8]) -> String {
        let token = Uuid::new_v4().to_string();
        let spool = job_root.join("sources").join(&token);
        fs::create_dir_all(&spool).unwrap();
        fs::write(spool.join("source.risudat"), bytes).unwrap();
        let ownership = json!({
            "format": "risunest-android-saf-spool", "version": 1, "token": token, "createdAtMillis": 1,
        });
        fs::write(spool.join("ownership.json"), ownership.to_string()).unwrap();
        let manifest = json!({
            "token": token, "state": "ready", "displayName": display_name, "bytes": bytes.len(),
            "totalBytes": bytes.len(), "importDestination": null,
        });
        fs::write(spool.join("source.json"), manifest.to_string()).unwrap();
        token
    }

    #[test]
    fn golden_samples_classify_by_content_then_by_name() {
        let character = || content("character", "json");
        let module = || content("module", "json");
        let lorebook = || content("lorebook", "json");
        let preset = || content("preset", "json");
        let unsupported = || refused("unsupported-format");
        let many_chunks: Vec<Vec<u8>> = (0..2_000)
            .map(|_| chunk(b"IDAT", &[0]))
            .chain([text("chara", CARD_TEXT)])
            .collect();
        let rows: Vec<(&str, Vec<u8>, Value)> = vec![
            ("ccv3.png", image([text("ccv3", CARD_TEXT)]), content("character", "png")),
            ("chara.png", image([text("chara", CARD_TEXT)]), content("character", "png")),
            (
                "both.png",
                image([text("chara", CARD_TEXT), text("ccv3", CARD_TEXT)]),
                content("character", "png"),
            ),
            ("late.png", image(many_chunks), content("character", "png")),
            ("persona.png", image([text("persona", CARD_TEXT)]), refused("png-without-card")),
            ("plain.png", image([]), refused("png-without-card")),
            ("cased.png", image([text("Chara", CARD_TEXT)]), refused("png-without-card")),
            (
                "asset.png",
                image([text("chara-ext-asset_:0", CARD_TEXT)]),
                refused("png-without-card"),
            ),
            (
                "after-end.png",
                [image([]), text("chara", CARD_TEXT)].concat(),
                refused("png-without-card"),
            ),
            ("card.charx", charx(), content("character", "charx")),
            ("renamed.zip", charx(), content("character", "charx")),
            (
                "no-card.charx",
                zip(&[("other.json", b"{}")], zip::CompressionMethod::Deflated),
                unsupported(),
            ),
            ("appended.jpg", [JPEG, &charx()].concat(), content("character", "jpeg-charx")),
            ("plain.jpeg", JPEG.to_vec(), refused("plain-jpeg")),
            ("module.risum", vec![0x6f, 0, 4, 0, 0, 0], content("module", "risum")),
            ("preset.risup", RISUP.to_vec(), content("preset", "risup")),
            (".risup", RISUP.to_vec(), content("preset", "risup")),
            ("PRESET.RISUPRESET", gzip(b"synthetic"), content("preset", "risupreset")),
            ("plugin.js", b"//@name synthetic\n".to_vec(), content("script", "js")),
            ("plugin.ts", b"//@name synthetic\n".to_vec(), content("script", "ts")),
            (
                "v2.json",
                br#"{"spec":"chara_card_v2","data":{"name":"Synthetic"}}"#.to_vec(),
                character(),
            ),
            (
                "v3.json",
                br#"{"data":{"name":"Synthetic"},"spec":"chara_card_v3"}"#.to_vec(),
                character(),
            ),
            (
                "off-spec.json",
                br#"{"char_name":"A","char_persona":"B","char_greeting":"C"}"#.to_vec(),
                character(),
            ),
            (
                "tavern.json",
                br#"{"name":"A","description":"B","first_mes":[]}"#.to_vec(),
                character(),
            ),
            (
                "falsy-greeting.json",
                br#"{"name":"A","description":"B","char_greeting":"","first_mes":0}"#.to_vec(),
                unsupported(),
            ),
            (
                "module.json",
                br#"{"type":"risuModule","name":"M","id":"x"}"#.to_vec(),
                module(),
            ),
            ("lorebook.json", br#"{"type":"risu","data":[]}"#.to_vec(), lorebook()),
            ("external.json", br#"{"entries":{}}"#.to_vec(), lorebook()),
            ("empty-entries.json", br#"{"entries":[]}"#.to_vec(), lorebook()),
            ("zero-entries.json", br#"{"entries":0}"#.to_vec(), unsupported()),
            ("hypa.json", br#"{"type":"risu","data":{"summaries":[]}}"#.to_vec(), unsupported()),
            ("regex.json", br#"{"type":"regex","data":[]}"#.to_vec(), module()),
            ("empty-regex.json", br#"{"type":"regex","data":""}"#.to_vec(), unsupported()),
            ("chat.json", br#"{"type":"risuChat","ver":1,"data":{}}"#.to_vec(), unsupported()),
            ("chats.json", br#"{"type":"risuAllChats","data":[]}"#.to_vec(), unsupported()),
            (
                "novelai.json",
                br#"{"presetVersion":3,"parameters":{"temperature":1}}"#.to_vec(),
                preset(),
            ),
            (
                "novelai-text.json",
                br#"{"presetVersion":"3","parameters":{}}"#.to_vec(),
                unsupported(),
            ),
            (
                "novelai-old.json",
                br#"{"presetVersion":2,"parameters":{}}"#.to_vec(),
                unsupported(),
            ),
            (
                "tavern-preset.json",
                br#"{"prompt_order":[{"order":[]}],"prompts":[]}"#.to_vec(),
                preset(),
            ),
            (
                "tavern-object.json",
                br#"{"prompt_order":{},"prompts":[]}"#.to_vec(),
                unsupported(),
            ),
            ("bot.json", br#"{"name":"Bot","mainPrompt":null}"#.to_vec(), preset()),
            ("array.json", br#"[{"spec":"chara_card_v2"}]"#.to_vec(), unsupported()),
            ("empty-object.json", b"{}".to_vec(), unsupported()),
            (
                "bom.json",
                b"\xef\xbb\xbf \r\n{\"spec\":\"chara_card_v2\",\"data\":{}}\n".to_vec(),
                unsupported(),
            ),
            (
                "whitespace.json",
                b" \r\n\t{\"spec\":\"chara_card_v2\",\"data\":{}}\n".to_vec(),
                character(),
            ),
            (
                "regex-entries.json",
                br#"{"type":"regex","data":[],"entries":{}}"#.to_vec(),
                lorebook(),
            ),
            (
                "repeated.json",
                br#"{"spec":"chara_card_v3","spec":1}"#.to_vec(),
                unsupported(),
            ),
            (
                "replaced.json",
                br#"{"spec":1,"spec":"chara_card_v2"}"#.to_vec(),
                character(),
            ),
            ("nested.json", br#"{"data":{"spec":"chara_card_v2"}}"#.to_vec(), unsupported()),
            ("truncated.json", br#"{"spec":"chara_card_v2""#.to_vec(), unsupported()),
            ("trailing.json", br#"{"spec":"chara_card_v2"} x"#.to_vec(), unsupported()),
            ("empty.json", Vec::new(), unsupported()),
            (
                "content-first.risup",
                br#"{"spec":"chara_card_v2"}"#.to_vec(),
                character(),
            ),
            ("backup.risunest", product_portable(), content("backup", "portable")),
            ("product.zip", product_portable(), content("backup", "portable")),
            ("portable.zip", portable(), content("backup", "portable")),
            (
                "backup.risudat",
                b"RISUSAVE\0synthetic".to_vec(),
                content("backup", "block-risu-save"),
            ),
            (
                "magic.json",
                b"RISUSAVE\0synthetic".to_vec(),
                content("backup", "block-risu-save"),
            ),
            (
                "compressed.risudat",
                zlib(br#"{"characters":[]}"#),
                content("backup", "block-risu-save"),
            ),
            ("compressed.json", zlib(br#"{"characters":[]}"#), unsupported()),
            (
                "card.risudat",
                image([text("chara", CARD_TEXT)]),
                content("character", "png"),
            ),
            ("card.risunest", charx(), content("character", "charx")),
            (
                "local.bin",
                b"\x04\0\0\0test\0\0\0\0".to_vec(),
                content("backup", "local-backup"),
            ),
            ("unknown.dat", vec![0, 1, 2, 3, 0xfe, 0xff, 0x80, 0x7f], unsupported()),
        ];

        let directory = tempfile::tempdir().unwrap();
        let files: Vec<(&str, Vec<u8>)> =
            rows.iter().map(|(name, bytes, _)| (*name, bytes.clone())).collect();
        let response = respond(directory.path(), desktop_sources(directory.path(), &files));
        assert_eq!(response.len(), rows.len());
        let mismatches: Vec<String> = rows
            .iter()
            .zip(&response)
            .filter_map(|((name, bytes, expected), actual)| {
                let expected = with_bytes(expected.clone(), bytes.len());
                (&expected != actual).then(|| format!("{name}: expected {expected}, got {actual}"))
            })
            .collect();
        assert!(mismatches.is_empty(), "{mismatches:#?}");
    }

    #[test]
    fn a_json_object_over_the_import_limit_is_refused_unread() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("huge.json");
        let mut file = File::create(&path).unwrap();
        file.write_all(br#"{"spec":"chara_card_v2","description":""#).unwrap();
        file.set_len(JSON_MAX_BYTES + 1).unwrap();
        drop(file);
        let small = br#"{"spec":"chara_card_v2","data":{}}"#.to_vec();
        let mut sources = vec![json!({ "source": { "type": "desktopPath", "path": path }, "displayName": "huge.json" })];
        sources.extend(desktop_sources(directory.path(), &[("card.json", small.clone())]));

        let response = respond(directory.path(), sources);

        assert_eq!(
            response[0],
            json!({ "kind": "unsupported", "refusal": "too-large", "bytes": JSON_MAX_BYTES + 1 }),
        );
        assert_eq!(response[1], with_bytes(content("character", "json"), small.len()));
    }

    #[test]
    fn an_unreadable_source_is_refused_alone_in_request_order() {
        let directory = tempfile::tempdir().unwrap();
        let card = br#"{"spec":"chara_card_v2","data":{}}"#.to_vec();
        let mut sources = desktop_sources(directory.path(), &[("card.json", card.clone())]);
        let missing = directory.path().join("missing.json");
        sources.push(json!({ "source": { "type": "desktopPath", "path": missing }, "displayName": "missing.json" }));
        sources.push(json!({ "source": { "type": "desktopPath", "path": directory.path() }, "displayName": "folder.json" }));
        sources.push(json!({ "source": { "type": "iosScoped", "token": "synthetic" }, "displayName": "card.json" }));
        sources.extend(desktop_sources(&directory.path().join("again"), &[("card.json", card.clone())]));

        let response = respond(directory.path(), sources);

        let character = with_bytes(content("character", "json"), card.len());
        assert_eq!(response.len(), 5);
        assert_eq!(response[0], character);
        assert_eq!(response[4], character);
        for refused in &response[1..4] {
            let message = refused["message"].as_str().unwrap_or_default();
            assert!(!message.is_empty(), "{refused}");
            assert_eq!(
                refused,
                &json!({ "kind": "unsupported", "refusal": "read-failed", "message": message }),
            );
        }
    }

    #[test]
    fn android_spools_classify_by_display_name_and_stay_claimable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let card = image([text("ccv3", CARD_TEXT)]);
        let compressed = zlib(br#"{"characters":[]}"#);
        let spools = [
            ("card.png", card.clone(), content("character", "png")),
            ("preset.risup", RISUP.to_vec(), content("preset", "risup")),
            // The spool file is always `source.risudat`; only its display name may make it a backup.
            ("notes.txt", compressed.clone(), refused("unsupported-format")),
            ("backup.risudat", compressed, content("backup", "block-risu-save")),
        ]
        .map(|(name, bytes, expected)| (write_spool(root, name, &bytes), name, bytes, expected));
        let sources = spools
            .iter()
            .map(|(token, name, _, _)| {
                json!({ "source": { "type": "androidSpool", "token": token }, "displayName": name })
            })
            .collect();

        let response = respond(root, sources);

        let expected: Vec<Value> = spools
            .iter()
            .map(|(_, _, bytes, expected)| with_bytes(expected.clone(), bytes.len()))
            .collect();
        assert_eq!(response, expected);
        for (token, _, bytes, _) in &spools {
            assert_eq!(claim_spool_source_for_test(root, token).unwrap(), bytes.len() as u64);
        }
    }

    #[test]
    fn the_png_walk_ends_at_its_chunk_bound_and_at_malformed_lengths() {
        let walk = |bytes: &[u8], max_chunks: usize| {
            png_has_card(&mut Cursor::new(bytes), bytes.len() as u64, max_chunks).unwrap()
        };
        let late = image((0..5).map(|_| chunk(b"IDAT", &[0])).chain([text("ccv3", CARD_TEXT)]));
        // IHDR, six IDAT chunks, then the card text.
        assert!(walk(&late, 8));
        assert!(!walk(&late, 7));

        let header = chunk(b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
        let past_end = png([
            header.clone(),
            [&0xffff_ffff_u32.to_be_bytes()[..], b"IDAT"].concat(),
            text("ccv3", CARD_TEXT),
        ]);
        assert!(!walk(&past_end, usize::MAX));
        let short = png([
            header.clone(),
            [&64_u32.to_be_bytes()[..], b"tEXt", b"ccv3"].concat(),
        ]);
        assert!(!walk(&short, usize::MAX));
        let unterminated = png(std::iter::repeat_n(chunk(b"zzzz", &[]), 1_000));
        assert!(!walk(&unterminated, usize::MAX));
        assert!(!walk(PNG_SIGNATURE, usize::MAX));
    }
}
