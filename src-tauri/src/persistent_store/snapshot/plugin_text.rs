use super::{observe_native_cas_paths, observe_native_url, observe_text, retain_unscannable_record,
    cas_hash_from_physical_key, is_url_delimiter, AssetRootSet, CAS_PHYSICAL_PREFIX, COLD_STORAGE_HEADER, URL_SCHEMES};
use crate::persistent_store::{device_store::plugin_values::text_value, StoreResult};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};

const REFERENCE_BYTES: usize = 1024 * 1024;
const PREFIX_BYTES: usize = 32;

struct TextRoots<'a> {
    roots: &'a mut AssetRootSet,
    head: String,
    pending: String,
    long: bool,
    cold: u8,
}

impl<'a> TextRoots<'a> {
    fn new(roots: &'a mut AssetRootSet, cold: u8) -> Self {
        Self { roots, head: String::new(), pending: String::new(), long: false, cold }
    }
    fn push(&mut self, text: &str) {
        if !self.long {
            if self.head.len() + text.len() <= REFERENCE_BYTES { self.head.push_str(text); }
            else {
                self.long = true;
                if self.cold != 0 || self.head.starts_with("assets/") || self.head.starts_with(COLD_STORAGE_HEADER) {
                    retain_unscannable_record(self.roots);
                }
                self.head.clear();
            }
        }
        self.pending.push_str(text);
        self.scan(false);
    }
    fn scan(&mut self, last: bool) {
        let mut offset = 0;
        let mut keep = self.pending.len();
        while offset < self.pending.len() {
            let text = &self.pending[offset..];
            let bytes = text.as_bytes();
            let mut found = None;
            for (index, _) in text.char_indices() {
                let suffix = &text[index..];
                let kind = if suffix.starts_with(CAS_PHYSICAL_PREFIX) { Some(0) }
                    else if ["{{inlay::", "{{inlayed::", "{{inlayeddata::"].iter().any(|prefix| suffix.starts_with(prefix)) { Some(1) }
                    else if matches!(bytes[index], b'r' | b'R' | b'h' | b'H') && URL_SCHEMES.iter().any(|scheme|
                        bytes.get(index..index + scheme.len()).is_some_and(|value| value.eq_ignore_ascii_case(scheme.as_bytes()))) { Some(2) }
                    else { None };
                if let Some(kind) = kind { found = Some((index, kind)); break; }
            }
            let Some((index, kind)) = found else {
                keep = self.pending.len().saturating_sub(PREFIX_BYTES);
                while !self.pending.is_char_boundary(keep) { keep += 1; }
                break;
            };
            let start = offset + index;
            let candidate = &self.pending[start..];
            let end = match kind {
                0 if candidate.len() > CAS_PHYSICAL_PREFIX.len() + 65 || last => Some((CAS_PHYSICAL_PREFIX.len() + 65).min(candidate.len())),
                1 => candidate.find("}}").map(|end| end + 2),
                2 => candidate.find(is_url_delimiter).or_else(|| last.then_some(candidate.len())),
                _ => None,
            };
            let Some(end) = end else {
                if candidate.len() > REFERENCE_BYTES {
                    retain_unscannable_record(self.roots);
                    offset = start + PREFIX_BYTES;
                    while !self.pending.is_char_boundary(offset) { offset += 1; }
                    continue;
                }
                keep = start; break;
            };
            match kind {
                0 => {
                    if candidate.as_bytes().get(end).is_none_or(|byte| !byte.is_ascii_hexdigit() && *byte != b'/') {
                        if let Some(hash) = cas_hash_from_physical_key(&candidate.as_bytes()[..end]) { self.roots.object_hashes.insert(hash); }
                    }
                }
                1 => { observe_text(&candidate[..end], self.roots); }
                _ => { observe_native_url(&candidate[..end], self.roots); observe_native_cas_paths(&candidate[..end], self.roots); }
            }
            offset = start + if kind == 0 { 1 } else { end.max(1) };
            while !self.pending.is_char_boundary(offset) { offset += 1; }
            keep = offset;
        }
        self.pending.drain(..keep);
    }
    fn finish(mut self) {
        self.scan(true);
        if !self.long {
            observe_text(&self.head, self.roots);
            if crate::trust_boundary::is_lower_hex_256(&self.head) { self.roots.object_hashes.insert(self.head.clone()); }
            if self.cold == 2 || (self.cold == 1 && !self.head.is_empty()) { self.roots.cold_keys.insert(self.head); }
        }
    }
}

pub(super) fn scan_raw(input: impl Read, roots: &mut AssetRootSet, cold: u8) -> io::Result<()> {
    let mut scan = TextRoots::new(roots, cold);
    text_value::visit_text(input, |text| { scan.push(text); Ok(()) })?;
    scan.finish(); Ok(())
}

fn invalid() -> io::Error { io::Error::new(io::ErrorKind::InvalidData, "plugin JSON is invalid") }
fn byte(input: &mut impl Read) -> io::Result<u8> { let mut byte = [0]; input.read_exact(&mut byte)?; Ok(byte[0]) }
fn space(input: &mut impl BufRead) -> io::Result<Option<u8>> {
    loop {
        let Some(&next) = input.fill_buf()?.first() else { return Ok(None); };
        if !next.is_ascii_whitespace() { return Ok(Some(next)); }
        input.consume(1);
    }
}

// The enclosing JSON has already been validated without building its values.
// This reader exposes one decoded string to the same bounded text scanner.
struct StringReader<'a, R> { input: &'a mut R, done: bool, pending: Vec<u8> }
impl<R: BufRead> Read for StringReader<'_, R> {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        if target.is_empty() { return Ok(0); }
        if !self.pending.is_empty() {
            let count = target.len().min(self.pending.len());
            target[..count].copy_from_slice(&self.pending[..count]); self.pending.drain(..count); return Ok(count);
        }
        if self.done { return Ok(0); }
        let available = self.input.fill_buf()?;
        if available.is_empty() { return Err(invalid()); }
        let count = available.iter().position(|byte| matches!(byte, b'"' | b'\\')).unwrap_or(available.len()).min(target.len());
        if count != 0 { target[..count].copy_from_slice(&available[..count]); self.input.consume(count); return Ok(count); }
        if byte(self.input)? == b'"' { self.done = true; return Ok(0); }
        let escape = byte(self.input)?;
        let decoded = match escape {
            b'"' | b'\\' | b'/' => escape, b'b' => 8, b'f' => 12, b'n' => 10, b'r' => 13, b't' => 9,
            b'u' => {
                let mut encoded = vec![b'"', b'\\', b'u'];
                for _ in 0..4 { encoded.push(byte(self.input)?); }
                if encoded[3] == b'd' || encoded[3] == b'D' {
                    if matches!(encoded[4], b'8' | b'9' | b'a' | b'A' | b'b' | b'B') {
                        for _ in 0..6 { encoded.push(byte(self.input)?); }
                    }
                }
                encoded.push(b'"');
                let text: String = serde_json::from_slice(&encoded).map_err(|_| invalid())?;
                self.pending.extend_from_slice(text.as_bytes()); return self.read(target);
            }
            _ => return Err(invalid()),
        };
        target[0] = decoded; Ok(1)
    }
}

fn json_value(input: &mut impl BufRead, roots: &mut AssetRootSet, cold: u8, depth: usize) -> io::Result<()> {
    if depth >= 128 { return Err(invalid()); }
    match space(input)?.ok_or_else(invalid)? {
        b'"' => { input.consume(1); scan_raw(StringReader { input, done: false, pending: vec![] }, roots, cold)?; }
        b'[' => {
            input.consume(1);
            if space(input)? == Some(b']') { input.consume(1); return Ok(()); }
            loop {
                json_value(input, roots, cold, depth + 1)?;
                match space(input)? { Some(b',') => input.consume(1), Some(b']') => { input.consume(1); break; }, _ => return Err(invalid()) }
            }
        }
        b'{' => {
            input.consume(1);
            if space(input)? == Some(b'}') { input.consume(1); return Ok(()); }
            loop {
                if space(input)? != Some(b'"') { return Err(invalid()); } input.consume(1);
                let mut key = Vec::new();
                let mut reader = StringReader { input: &mut *input, done: false, pending: vec![] };
                (&mut reader).take(64).read_to_end(&mut key)?;
                io::copy(&mut reader, &mut io::sink())?;
                if space(input)? != Some(b':') { return Err(invalid()); } input.consume(1);
                let cold = match key.as_slice() { b"coldstorage" => 1, b"coldStoragedChats" => 2, _ => 0 };
                json_value(input, roots, cold, depth + 1)?;
                match space(input)? { Some(b',') => input.consume(1), Some(b'}') => { input.consume(1); break; }, _ => return Err(invalid()) }
            }
        }
        _ => loop {
            let available = input.fill_buf()?;
            let count = available.iter().position(|byte| byte.is_ascii_whitespace() || matches!(byte, b',' | b']' | b'}')).unwrap_or(available.len());
            let end = count < available.len() || available.is_empty(); input.consume(count);
            if end { break; }
        },
    }
    Ok(())
}

pub(super) fn scan(mut input: impl Read + Seek, roots: &mut AssetRootSet) -> StoreResult<()> {
    let mut magic = [0; 5];
    let count = input.read(&mut magic)?;
    input.seek(SeekFrom::Start(0))?;
    if count == 5 && magic == [0, 80, 76, 83, 1] { return scan_binary(input, roots); }
    let valid = serde_json::from_reader::<_, serde::de::IgnoredAny>(BufReader::new(&mut input)).is_ok();
    input.seek(SeekFrom::Start(0))?;
    if valid { json_value(&mut BufReader::new(input), roots, 0, 0)?; }
    else { scan_raw(input, roots, 0)?; }
    Ok(())
}

pub(super) fn scan_binary(mut input: impl Read + Seek, roots: &mut AssetRootSet) -> StoreResult<()> {
    let info = crate::persistent_store::device_store::plugin_values::raw_value::inspect(&mut input)?;
    input.seek(SeekFrom::Start(0))?;
    let mut header = [0; 13];
    input.read_exact(&mut header)?;
    let lengths = [u32::from_le_bytes(header[5..9].try_into().unwrap()), u32::from_le_bytes(header[9..13].try_into().unwrap())];
    let mut offset = 13;
    for length in lengths {
        serde_json::from_reader::<_, serde::de::IgnoredAny>(BufReader::new((&mut input).take(length as u64))).map_err(io::Error::other)?;
        input.seek(SeekFrom::Start(offset))?;
        json_value(&mut BufReader::new((&mut input).take(length as u64)), roots, 0, 0)?;
        offset += length as u64;
        input.seek(SeekFrom::Start(offset))?;
    }
    if offset > info.byte_length { return Err(invalid().into()); }
    Ok(())
}

pub(super) fn scan_encoded(input: impl Read, roots: &mut AssetRootSet, limit: u64) -> StoreResult<text_value::TextInfo> {
    struct Output<'a> { scan: TextRoots<'a>, pending: Vec<u8> }
    impl io::Write for Output<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.pending.extend_from_slice(bytes);
            let valid = match std::str::from_utf8(&self.pending) {
                Ok(_) => self.pending.len(),
                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                Err(_) => return Err(invalid()),
            };
            self.scan.push(std::str::from_utf8(&self.pending[..valid]).map_err(|_| invalid())?);
            self.pending.drain(..valid); Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { if self.pending.is_empty() { Ok(()) } else { Err(invalid()) } }
    }
    let mut output = Output { scan: TextRoots::new(roots, 0), pending: vec![] };
    let info = text_value::decode(input, &mut output, limit)?;
    output.scan.finish(); Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn ranged_values_keep_json_escaped_and_cross_boundary_asset_roots() {
        let hash = "ab".repeat(32);
        let physical = format!("assets/objects/{}/{}", &hash[..2], &hash[2..]);
        let url = format!("HTTPS://RISUASSET.LOCALHOST/{}", hex::encode(&physical));
        let text = format!("{}가😀{} {} {{{{inlay::synthetic-id}}}} {}", "x".repeat(REFERENCE_BYTES + 65_520), physical, url, physical);
        let value = serde_json::json!({"huge": text, "coldstorage":["", "cold-a"], "coldStoragedChats":["", "cold-b"], "hash":hash, "escaped":"{{inlay::한😀}}"});
        let mut expected = AssetRootSet::default();
        super::super::observe_json_value(&value, None, &mut expected);
        let encoded = serde_json::to_string(&value).unwrap().replace("한😀", "\\uD55C\\uD83D\\uDE00");
        let mut actual = AssetRootSet::default();
        scan(Cursor::new(encoded.as_bytes()), &mut actual).unwrap();
        assert_eq!(actual, expected);
        let mut expected = AssetRootSet::default();
        super::super::observe_json_value(&serde_json::Value::String(text.clone()), None, &mut expected);
        for width in [7, 79, 65_536] {
            let mut actual = AssetRootSet::default();
            let mut observer = TextRoots::new(&mut actual, 0);
            let mut offset = 0;
            while offset < text.len() {
                let mut end = (offset + width).min(text.len());
                while !text.is_char_boundary(end) { end -= 1; }
                observer.push(&text[offset..end]); offset = end;
            }
            observer.finish(); assert_eq!(actual, expected, "width={width}");
        }
        let mut actual = AssetRootSet::default();
        let encoded = serde_json::to_vec(&text).unwrap();
        assert_eq!(scan_encoded(encoded.as_slice(), &mut actual, text_value::MAX_VALUE_BYTES).unwrap().hash, risunest_sync_wire::hash(&encoded));
        assert_eq!(actual, expected);
    }
}
