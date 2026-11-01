//! Structure-form values: JSON-space text that starts with the marker line and
//! names its byte parts first, so a reader learns the parts from a short
//! prefix. The parts are device objects; the renderer interprets the nodes.
use super::super::invalid;
use super::text_value;
use crate::persistent_store::{message_pages, StoreResult};
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read};
use std::time::{Duration, Instant};

pub(crate) const MARKER: &[u8] = b"#risunest-structured:1\n";
pub(crate) const PART_BYTES: u64 = 8 * 1024 * 1024;
/// The largest piece one part read returns.
pub(crate) const PART_READ_BYTES: usize = 1024 * 1024;
const MAX_PARTS: usize = (text_value::MAX_VALUE_BYTES / PART_BYTES) as usize;
/// Rows shorter than the marker and an empty part list cannot be structure form.
pub(crate) const MIN_TEXT_BYTES: i64 = (MARKER.len() + r#"{"parts":[],"bytes":0,"nodes":[],"root":null}"#.len()) as i64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StructurePrefix {
    pub(crate) parts: Vec<String>,
    pub(crate) bytes: u64,
}

impl StructurePrefix {
    pub(crate) fn part_length(&self, index: usize) -> u64 {
        if index + 1 < self.parts.len() { PART_BYTES } else { self.bytes - PART_BYTES * index as u64 }
    }
}

fn malformed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "plugin structure value is malformed")
}

fn expect(input: &mut impl BufRead, expected: &[u8]) -> io::Result<()> {
    let mut actual = vec![0; expected.len()];
    input.read_exact(&mut actual).map_err(|_| malformed())?;
    if actual != expected { return Err(malformed()); }
    Ok(())
}

fn peek(input: &mut impl BufRead) -> io::Result<u8> {
    input.fill_buf()?.first().copied().ok_or_else(malformed)
}

/// Reads the marker and the part list. `None` means the text is not structure
/// form; a text that starts with the marker but has no valid prefix is an error.
pub(crate) fn read_prefix(input: impl Read) -> io::Result<Option<StructurePrefix>> {
    prefix(&mut BufReader::with_capacity(8 * 1024, input))
}

/// The prefix and the root's type name. The host writes the root as the first
/// node with its type first, so this reads only a few bytes past the prefix.
pub(crate) fn read_summary(input: impl Read) -> io::Result<Option<(StructurePrefix, String)>> {
    let mut input = BufReader::with_capacity(8 * 1024, input);
    let Some(prefix) = prefix(&mut input)? else { return Ok(None) };
    expect(&mut input, br#"[{"type":""#)?;
    let mut name = word(&mut input)?;
    if name == "View" || name == "Primitive" || name == "Box" {
        expect(&mut input, br#","name":""#)?;
        name = word(&mut input)?;
    }
    Ok(Some((prefix, name)))
}

fn word(input: &mut impl BufRead) -> io::Result<String> {
    let mut word = String::new();
    loop {
        let byte = peek(input)?;
        input.consume(1);
        if byte == b'"' && !word.is_empty() { return Ok(word); }
        if !byte.is_ascii_alphanumeric() || word.len() == 32 { return Err(malformed()); }
        word.push(byte as char);
    }
}

fn prefix(input: &mut impl BufRead) -> io::Result<Option<StructurePrefix>> {
    let mut marker = Vec::with_capacity(MARKER.len());
    input.take(MARKER.len() as u64).read_to_end(&mut marker)?;
    if marker != MARKER { return Ok(None); }
    expect(input, br#"{"parts":["#)?;
    let mut parts = Vec::new();
    if peek(input)? == b']' {
        input.consume(1);
    } else {
        loop {
            if parts.len() == MAX_PARTS { return Err(malformed()); }
            expect(input, br#""p:"#)?;
            let mut hash = [0; 64];
            input.read_exact(&mut hash).map_err(|_| malformed())?;
            if !hash.iter().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')) { return Err(malformed()); }
            expect(input, b"\"")?;
            parts.push(String::from_utf8(hash.to_vec()).map_err(|_| malformed())?);
            let mut separator = [0];
            input.read_exact(&mut separator).map_err(|_| malformed())?;
            match separator[0] { b',' => continue, b']' => break, _ => return Err(malformed()) }
        }
    }
    expect(input, br#","bytes":"#)?;
    let mut digits = Vec::new();
    while peek(input)?.is_ascii_digit() {
        if digits.len() == 16 { return Err(malformed()); }
        digits.push(peek(input)?);
        input.consume(1);
    }
    if digits.is_empty() || (digits.len() > 1 && digits[0] == b'0') { return Err(malformed()); }
    let bytes: u64 = std::str::from_utf8(&digits).ok().and_then(|text| text.parse().ok()).ok_or_else(malformed)?;
    expect(input, br#","nodes":"#)?;
    if bytes > text_value::MAX_VALUE_BYTES || parts.len() as u64 != bytes.div_ceil(PART_BYTES) { return Err(malformed()); }
    Ok(Some(StructurePrefix { parts, bytes }))
}

/// Accepts the whole text when it is the marker line and one JSON object, and
/// returns its prefix.
pub(crate) fn validate_text<R: Read>(mut source: impl FnMut() -> StoreResult<R>) -> StoreResult<Option<StructurePrefix>> {
    let Some(prefix) = read_prefix(source()?)? else { return Ok(None) };
    let mut input = source()?;
    io::copy(&mut (&mut input).take(MARKER.len() as u64), &mut io::sink())?;
    serde_json::from_reader::<_, serde::de::IgnoredAny>(BufReader::new(input))
        .map_err(|_| invalid("plugin structure value is malformed"))?;
    Ok(Some(prefix))
}

/// Checks the stored size (the text's encoded length plus the part bytes)
/// against `limit` and that every part is a device object of the right length,
/// then clears any collection mark on them. Runs inside the transaction that
/// commits the row, so the parts stay referenced from then on.
pub(crate) fn admit_parts(db: &Connection, prefix: &StructurePrefix, encoded_text: u64, limit: u64) -> StoreResult<()> {
    if encoded_text.checked_add(prefix.bytes).is_none_or(|size| size > limit) {
        return Err(invalid("plugin device value is too large"));
    }
    for (index, hash) in prefix.parts.iter().enumerate() {
        let length: Option<i64> = db.query_row(
            "SELECT length(o.body) FROM message_page_objects o JOIN message_page_verified_objects v ON v.hash=o.hash WHERE o.hash=?1",
            [hash], |row| row.get(0)).optional()?;
        if length.and_then(|length| u64::try_from(length).ok()) != Some(prefix.part_length(index)) {
            return Err(invalid("plugin structure part is missing"));
        }
        db.execute("DELETE FROM message_page_object_marks WHERE hash=?1", [hash])?;
    }
    Ok(())
}

/// Enough of a decoded text for any prefix: the marker, at most `MAX_PARTS`
/// part names and the byte count.
const HEAD_BYTES: usize = 8 * 1024;

struct Head(Vec<u8>);
impl io::Write for Head {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let room = HEAD_BYTES - self.0.len();
        self.0.extend_from_slice(&bytes[..bytes.len().min(room)]);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

/// Decodes a text object in the JSON string encoding once, returning its
/// measures and the prefix of the text it holds.
pub(crate) fn object_prefix(db: &Connection, hash: &str) -> StoreResult<(text_value::TextInfo, Option<StructurePrefix>)> {
    let (rowid, length) = text_value::object_row(db, hash)?;
    if length > text_value::MAX_VALUE_BYTES { return Err(invalid("plugin device value is too large")); }
    let mut head = Head(Vec::new());
    let blob = db.blob_open(rusqlite::MAIN_DB, "message_page_objects", "body", rowid, true)?;
    let info = text_value::decode(blob, &mut head, text_value::MAX_VALUE_BYTES)?;
    Ok((info, read_prefix(&head.0[..])?))
}

/// The parts a stored row names, or `None` for a plain row.
pub(crate) fn row_prefix(db: &Connection, schema: &str, rowid: i64) -> StoreResult<Option<StructurePrefix>> {
    let blob = db.blob_open(schema, "plugin_device_storage", "value", rowid, true)?;
    Ok(read_prefix(blob)?)
}

pub(crate) fn row_summary(db: &Connection, rowid: i64) -> StoreResult<Option<(StructurePrefix, String)>> {
    let blob = db.blob_open(rusqlite::MAIN_DB, "plugin_device_storage", "value", rowid, true)?;
    Ok(read_summary(blob)?)
}

/// Every part that a live structure row in `schema` names.
pub(crate) fn row_parts(db: &Connection, schema: &str, mut visit: impl FnMut(&str) -> StoreResult<()>) -> StoreResult<()> {
    let mut statement = db.prepare(&format!(
        "SELECT rowid FROM {schema}.plugin_device_storage WHERE tombstone=0 AND space='json' AND value IS NOT NULL AND byte_size>=?1"))?;
    let rows = statement.query_map([MIN_TEXT_BYTES], |row| row.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?;
    for rowid in rows {
        if let Some(prefix) = row_prefix(db, schema, rowid)? {
            for part in &prefix.parts { visit(part)?; }
        }
    }
    Ok(())
}

pub(crate) fn read_part(db: &Connection, hash: &str, offset: u64) -> StoreResult<Vec<u8>> {
    risunest_sync_wire::validate_hash(hash).map_err(|_| invalid("plugin structure part is invalid"))?;
    let row: Option<(i64, i64)> = db.query_row(
        "SELECT o.rowid,length(o.body) FROM message_page_objects o JOIN message_page_verified_objects v ON v.hash=o.hash WHERE o.hash=?1",
        [hash], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
    let Some((rowid, length)) = row else { return Err(invalid("plugin structure part is missing")) };
    let length = u64::try_from(length).map_err(|_| invalid("plugin structure part is invalid"))?;
    if offset >= length || length > PART_BYTES { return Err(invalid("plugin structure part range is invalid")); }
    let blob = db.blob_open(rusqlite::MAIN_DB, "message_page_objects", "body", rowid, true)?;
    let mut bytes = vec![0; (length - offset).min(PART_READ_BYTES as u64) as usize];
    blob.read_at_exact(&mut bytes, offset as usize)?;
    Ok(bytes)
}

const IDLE_LIFETIME: Duration = Duration::from_secs(5 * 60);
const OWNER_UPLOADS: usize = 2;
const TOTAL_UPLOADS: usize = 8;
const MAX_APPEND_BYTES: usize = 1024 * 1024;

struct PartUpload {
    owner: String,
    bytes: Vec<u8>,
    expected: usize,
    touched: Instant,
    _receiver: Option<std::sync::Arc<crate::asset_byte_transport::AndroidReceiver>>,
}

/// Parts arrive in bounded pieces and become device objects once complete.
#[derive(Default)]
pub(crate) struct PartUploads { uploads: HashMap<String, PartUpload> }

impl PartUploads {
    fn expire_at(&mut self, now: Instant) {
        self.uploads.retain(|_, upload| now.duration_since(upload.touched) < IDLE_LIFETIME);
    }

    pub(crate) fn open(&mut self, owner: &str, byte_length: u64) -> StoreResult<String> {
        if owner.is_empty() || byte_length == 0 || byte_length > PART_BYTES { return Err(invalid("plugin structure part is invalid")); }
        self.expire_at(Instant::now());
        if self.uploads.len() >= TOTAL_UPLOADS || self.uploads.values().filter(|upload| upload.owner == owner).count() >= OWNER_UPLOADS {
            return Err(invalid("plugin structure part uploads are full"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        self.uploads.insert(id.clone(), PartUpload { owner: owner.into(), bytes: Vec::with_capacity(byte_length as usize),
            expected: byte_length as usize, touched: Instant::now(), _receiver: None });
        Ok(id)
    }

    #[cfg(target_os = "android")]
    pub(crate) fn attach_receiver(&mut self, id: &str, receiver: std::sync::Arc<crate::asset_byte_transport::AndroidReceiver>) -> StoreResult<()> {
        let upload = self.uploads.get_mut(id).ok_or_else(|| invalid("plugin structure part upload is unavailable"))?;
        upload._receiver = Some(receiver);
        Ok(())
    }

    pub(crate) fn append(&mut self, id: &str, offset: u64, bytes: &[u8]) -> StoreResult<u64> {
        self.expire_at(Instant::now());
        let upload = self.uploads.get_mut(id).ok_or_else(|| invalid("plugin structure part upload is unavailable"))?;
        if bytes.is_empty() || bytes.len() > MAX_APPEND_BYTES || offset != upload.bytes.len() as u64
            || upload.bytes.len() + bytes.len() > upload.expected {
            self.uploads.remove(id);
            return Err(invalid("plugin structure part piece is invalid"));
        }
        upload.bytes.extend_from_slice(bytes);
        upload.touched = Instant::now();
        Ok(upload.bytes.len() as u64)
    }

    pub(crate) fn abort(&mut self, owner: &str, id: &str) {
        if self.uploads.get(id).is_some_and(|upload| upload.owner == owner) { self.uploads.remove(id); }
    }

    /// Stores the finished part and returns its SHA-256. The object stays
    /// unreferenced until a row that names it commits.
    pub(crate) fn finish(&mut self, db: &mut Connection, owner: &str, id: &str) -> StoreResult<String> {
        self.expire_at(Instant::now());
        if !self.uploads.get(id).is_some_and(|upload| upload.owner == owner) {
            return Err(invalid("plugin structure part upload is unavailable"));
        }
        let upload = self.uploads.remove(id).unwrap();
        if upload.bytes.len() != upload.expected { return Err(invalid("plugin structure part is incomplete")); }
        let hash = risunest_sync_wire::hash(&upload.bytes);
        let transaction = db.transaction()?;
        message_pages::put_object(&transaction, &hash, &upload.bytes)?;
        transaction.commit()?;
        Ok(hash)
    }
}

impl super::super::DeviceStore {
    pub(crate) fn open_plugin_device_part(&mut self, owner: &str, byte_length: u64) -> StoreResult<String> {
        self.plugin_parts.open(owner, byte_length)
    }

    #[cfg(target_os = "android")]
    pub(crate) fn attach_plugin_device_part_receiver(&mut self, id: &str, receiver: std::sync::Arc<crate::asset_byte_transport::AndroidReceiver>) -> StoreResult<()> {
        self.plugin_parts.attach_receiver(id, receiver)
    }

    pub(crate) fn append_plugin_device_part(&mut self, id: &str, offset: u64, bytes: &[u8]) -> StoreResult<u64> {
        self.plugin_parts.append(id, offset, bytes)
    }

    pub(crate) fn finish_plugin_device_part(&mut self, owner: &str, id: &str) -> StoreResult<String> {
        self.plugin_parts.finish(&mut self.connection, owner, id)
    }

    pub(crate) fn abort_plugin_device_part(&mut self, owner: &str, id: &str) {
        self.plugin_parts.abort(owner, id)
    }

    pub(crate) fn read_plugin_device_part(&self, hash: &str, offset: u64) -> StoreResult<Vec<u8>> {
        read_part(&self.connection, hash, offset)
    }
}

/// Stores one part outside an upload, as restores and receives do.
pub(crate) fn put_part(db: &Connection, bytes: &[u8]) -> StoreResult<String> {
    if bytes.is_empty() || bytes.len() as u64 > PART_BYTES { return Err(invalid("plugin structure part is invalid")); }
    let hash = risunest_sync_wire::hash(bytes);
    message_pages::put_object(db, &hash, bytes)?;
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(parts: &[&str], bytes: u64) -> String {
        let parts = parts.iter().map(|part| format!("\"p:{part}\"")).collect::<Vec<_>>().join(",");
        format!("#risunest-structured:1\n{{\"parts\":[{parts}],\"bytes\":{bytes},\"nodes\":[{{\"type\":\"View\",\"name\":\"Uint8Array\",\"buffer\":1,\"byteOffset\":0,\"byteLength\":{bytes}}},{{\"type\":\"ArrayBuffer\",\"offset\":0,\"byteLength\":{bytes}}}],\"root\":{{\"ref\":0}}}}")
    }

    #[test]
    fn summarizes_the_root_type_from_the_first_node() {
        let hash = "a".repeat(64);
        let summary = read_summary(text(&[&hash], 1).as_bytes()).unwrap().unwrap();
        assert_eq!((summary.0.bytes, summary.1.as_str()), (1, "Uint8Array"));
        let body = |nodes: &str| format!("{}{{\"parts\":[],\"bytes\":0,\"nodes\":{nodes},\"root\":{{\"ref\":0}}}}", std::str::from_utf8(MARKER).unwrap());
        assert_eq!(read_summary(body(r#"[{"type":"Map","entries":[]}]"#).as_bytes()).unwrap().unwrap().1, "Map");
        assert_eq!(read_summary(body(r#"[{"type":"Primitive","name":"bigint","value":"1"}]"#).as_bytes()).unwrap().unwrap().1, "bigint");
        assert_eq!(read_summary(body(r#"[{"type":"Box","name":"String","value":"a"}]"#).as_bytes()).unwrap().unwrap().1, "String");
        assert_eq!(read_summary(b"{}".as_slice()).unwrap(), None);
        for broken in [r#"[]"#, r#"[{"name":"Map","type":"Map"}]"#, r#"[{"type":"a b"}]"#] {
            assert!(read_summary(body(broken).as_bytes()).is_err(), "{broken}");
        }
    }

    #[test]
    fn reads_the_part_list_from_the_prefix_and_rejects_malformed_structure_text() {
        let hash = "a".repeat(64);
        assert_eq!(read_prefix(text(&[], 0).as_bytes()).unwrap(), Some(StructurePrefix { parts: vec![], bytes: 0 }));
        let two = read_prefix(text(&[&hash, &hash], PART_BYTES + 1).as_bytes()).unwrap().unwrap();
        assert_eq!(two.parts, vec![hash.clone(), hash.clone()]);
        assert_eq!((two.part_length(0), two.part_length(1)), (PART_BYTES, 1));
        for plain in [r##""#risunest-structured:1\n""##, "{}", "#risunest", ""] {
            assert_eq!(read_prefix(plain.as_bytes()).unwrap(), None, "{plain}");
        }
        for broken in [text(&[&hash], 0), text(&[], 1), text(&[&"A".repeat(64)], 1), text(&[&hash], PART_BYTES + 1),
            text(&[&hash], text_value::MAX_VALUE_BYTES + 1), "#risunest-structured:1\n{\"parts\":[],\"bytes\":01,\"nodes\":[]}".into(),
            "#risunest-structured:1\n{\"bytes\":0,\"parts\":[],\"nodes\":[]}".into()] {
            assert!(read_prefix(broken.as_bytes()).is_err(), "{broken}");
        }
        let valid = text(&[&hash], 1);
        assert!(validate_text(|| Ok(Box::new(valid.as_bytes()))).unwrap().is_some());
        let truncated = &valid[..valid.len() - 1];
        assert!(validate_text(|| Ok(Box::new(truncated.as_bytes()))).is_err());
    }

    use crate::persistent_store::device_store::plugin_values::{admit_structure_row, PluginDeviceMutation};
    use crate::persistent_store::{MessageObjectStore, PersistentStore, ASSET_GC_PRODUCT_MINIMUM_GRACE_MS, MESSAGE_PAGE_SWEEP_LIMIT, SUPERSEDED_OBJECT_GRACE_MS};

    const OWNER: &str = "synthetic-owner";

    fn store() -> (tempfile::TempDir, PersistentStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = PersistentStore::open(directory.path()).unwrap();
        (directory, store)
    }

    fn upload(store: &mut PersistentStore, bytes: &[u8]) -> String {
        let device = store.device_store_mut().unwrap();
        let id = device.open_plugin_device_part(OWNER, bytes.len() as u64).unwrap();
        for (index, piece) in bytes.chunks(3).enumerate() {
            assert_eq!(device.append_plugin_device_part(&id, index as u64 * 3, piece).unwrap(), (index * 3 + piece.len()) as u64);
        }
        device.finish_plugin_device_part(OWNER, &id).unwrap()
    }

    fn set(store: &mut PersistentStore, space: &str, key: &str, value: String) -> StoreResult<()> {
        store.device_store_mut().unwrap().write_plugin_device_values(OWNER, &[PluginDeviceMutation::Set { space: space.into(), key: key.into(), value }])
    }

    fn read(store: &PersistentStore, key: &str) -> Option<String> {
        store.device_store().unwrap().read_plugin_device_value(OWNER, "json", key).unwrap()
    }

    fn object(store: &PersistentStore, table: &str, hash: &str) -> bool {
        store.device_store().unwrap().connection().query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE hash=?1)"), [hash], |row| row.get(0)).unwrap()
    }

    #[test]
    fn a_structure_row_commits_only_as_whole_text_with_its_parts_within_the_stored_size() {
        let (_directory, mut store) = store();
        let bytes = b"synthetic \"part\" bytes";
        let hash = upload(&mut store, bytes);
        assert_eq!(hash, risunest_sync_wire::hash(bytes));
        let stored = text(&[&hash], bytes.len() as u64);
        set(&mut store, "json", "kept", stored.clone()).unwrap();
        assert_eq!(read(&store, "kept"), Some(stored.clone()));
        let device = store.device_store().unwrap();
        assert_eq!(device.read_plugin_device_part(&hash, 9).unwrap(), bytes[9..]);
        assert!(device.read_plugin_device_part(&hash, bytes.len() as u64).is_err());
        assert!(device.read_plugin_device_part(&"b".repeat(64), 0).is_err());

        let missing = "b".repeat(64);
        for refused in [text(&[&missing], 1), text(&[&hash], bytes.len() as u64 + 1), stored[..stored.len() - 1].to_owned()] {
            assert!(set(&mut store, "json", "kept", refused.clone()).is_err(), "{refused}");
            assert_eq!(read(&store, "kept"), Some(stored.clone()));
        }
        // Marker-like text is plain wherever it cannot be structure form.
        set(&mut store, "json", "quoted", serde_json::to_string(&text(&[&missing], 1)).unwrap()).unwrap();
        set(&mut store, "string", "raw", text(&[&missing], 1)).unwrap();
        let listed = store.device_store().unwrap().list_plugin_device_storage().unwrap();
        let listed = listed.iter().map(|item| (item.key.as_str(), item.structure_type.as_deref(), item.byte_size)).collect::<Vec<_>>();
        assert_eq!(listed[0], ("kept", Some("Uint8Array"), (stored.len() + bytes.len()) as i64));
        assert!(listed[1].1.is_none() && listed[2].1.is_none());

        // The stored size counts the text as an encoded JSON string, escapes included.
        let size = serde_json::to_string(&stored).unwrap().len() as u64 + bytes.len() as u64;
        let device = store.device_store_mut().unwrap();
        let transaction = device.transaction().unwrap();
        admit_structure_row(&transaction, OWNER, "json", "kept", size).unwrap();
        assert!(admit_structure_row(&transaction, OWNER, "json", "kept", size - 1).is_err());
    }

    #[test]
    fn part_uploads_take_only_their_pieces_in_order_and_stay_bounded() {
        let mut uploads = PartUploads::default();
        assert!(uploads.open(OWNER, 0).is_err());
        assert!(uploads.open(OWNER, PART_BYTES + 1).is_err());
        let id = uploads.open(OWNER, 4).unwrap();
        assert!(uploads.append(&id, 1, b"a").is_err());
        assert!(uploads.append(&id, 0, b"a").is_err(), "a refused piece ends its upload");
        let id = uploads.open(OWNER, 4).unwrap();
        assert!(uploads.append(&id, 0, b"abcde").is_err());
        let ids = [uploads.open(OWNER, 1).unwrap(), uploads.open(OWNER, 1).unwrap()];
        assert!(uploads.open(OWNER, 1).is_err());
        uploads.abort("other-owner", &ids[0]);
        assert!(uploads.open(OWNER, 1).is_err());
        uploads.abort(OWNER, &ids[0]);
        uploads.open(OWNER, 1).unwrap();
        let (_directory, mut store) = store();
        let device = store.device_store_mut().unwrap();
        let id = device.open_plugin_device_part(OWNER, 2).unwrap();
        device.append_plugin_device_part(&id, 0, b"a").unwrap();
        assert!(device.finish_plugin_device_part("other-owner", &id).is_err());
        assert!(device.finish_plugin_device_part(OWNER, &id).is_err(), "an incomplete part is never stored");
    }

    #[test]
    fn parts_stay_while_a_row_names_them_and_are_collected_after_the_grace_once_none_does() {
        let (grace, limit, device) = (ASSET_GC_PRODUCT_MINIMUM_GRACE_MS, MESSAGE_PAGE_SWEEP_LIMIT, MessageObjectStore::Device);
        let (_directory, mut store) = store();
        let now = 1_900_000_000_000i64;
        let (first, second) = (b"first synthetic part".as_slice(), b"second synthetic part".as_slice());
        let first_hash = upload(&mut store, first);
        store.sweep_message_page_objects(device, now, limit).unwrap();
        assert!(object(&store, "message_page_object_marks", &first_hash), "an uploaded part waits for its row");
        set(&mut store, "json", "value", text(&[&first_hash], first.len() as u64)).unwrap();
        assert!(!object(&store, "message_page_object_marks", &first_hash));
        store.sweep_message_page_objects(device, now + grace, limit).unwrap();
        store.sweep_message_page_objects(device, now + 2 * grace, limit).unwrap();
        assert!(object(&store, "message_page_objects", &first_hash));
        assert!(!object(&store, "message_page_object_marks", &first_hash));

        let lease = store.lww_acquire_library_backup_capture(store.revision().unwrap()).unwrap();
        let second_hash = upload(&mut store, second);
        set(&mut store, "json", "value", text(&[&second_hash], second.len() as u64)).unwrap();
        store.sweep_message_page_objects(device, now + 3 * grace, limit).unwrap();
        store.sweep_message_page_objects(device, now + 4 * grace, limit).unwrap();
        assert!(object(&store, "message_page_objects", &first_hash), "a pinned device row still names the part");
        store.release_revision(&lease.lease).unwrap();
        store.sweep_message_page_objects(device, now + 5 * grace, limit).unwrap();
        assert!(object(&store, "message_page_object_marks", &first_hash));
        store.sweep_message_page_objects(device, now + 6 * grace, limit).unwrap();
        assert!(!object(&store, "message_page_objects", &first_hash), "the replaced part survived");
        assert!(object(&store, "message_page_objects", &second_hash));
        assert_eq!(store.device_store().unwrap().read_plugin_device_part(&second_hash, 0).unwrap(), second);

        // A part whose row never committed is collected like any unreferenced object.
        let orphan = upload(&mut store, b"orphan synthetic part");
        assert!(set(&mut store, "json", "value", text(&[&orphan, &missing_hash()], PART_BYTES + 1)).is_err());
        store.sweep_message_page_objects(device, now + 7 * grace, limit).unwrap();
        store.sweep_message_page_objects(device, now + 8 * grace, limit).unwrap();
        assert!(!object(&store, "message_page_objects", &orphan));
        assert!(object(&store, "message_page_objects", &second_hash));

        // A part a later value no longer names waits only the short grace, and
        // one whose row is not committed yet keeps the long one.
        let clock = || crate::persistent_store::commands::current_time_ms().unwrap();
        let third = b"third synthetic part".as_slice();
        let third_hash = upload(&mut store, third);
        let before = clock();
        set(&mut store, "json", "value", text(&[&third_hash], third.len() as u64)).unwrap();
        let after = clock();
        let pending = upload(&mut store, b"pending synthetic part");
        store.sweep_message_page_objects(device, before + SUPERSEDED_OBJECT_GRACE_MS - 60_000, limit).unwrap();
        assert!(object(&store, "message_page_objects", &second_hash), "a replaced part went before its grace");
        store.sweep_message_page_objects(device, after + SUPERSEDED_OBJECT_GRACE_MS, limit).unwrap();
        assert!(!object(&store, "message_page_objects", &second_hash), "a replaced part outlived the short grace");
        assert!(object(&store, "message_page_objects", &third_hash));
        assert!(object(&store, "message_page_objects", &pending), "an uploaded part lost the long grace");
        // A part the next value still names stays unmarked.
        let retyped = text(&[&third_hash], third.len() as u64).replace("Uint8Array", "Int8Array");
        set(&mut store, "json", "value", retyped.clone()).unwrap();
        assert_eq!(read(&store, "value"), Some(retyped));
        assert!(!object(&store, "message_page_object_marks", &third_hash), "a part the new value names was marked");
    }

    fn missing_hash() -> String { "c".repeat(64) }
}
