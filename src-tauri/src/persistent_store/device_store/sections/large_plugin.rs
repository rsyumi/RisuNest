use super::*;
use std::io::{self, Read, Seek, SeekFrom, Write};

pub(super) fn capture_binary<R: Read>(target: Option<&Connection>, mut source: impl FnMut() -> StoreResult<R>) -> StoreResult<SectionValueRow> {
    let object = match target {
        Some(target) => plugin_values::raw_value::store(target, &mut source)?,
        None => plugin_values::raw_value::inspect(source()?)?,
    };
    let mut input = source()?;
    let mut parts = vec![];
    loop {
        let mut bytes = vec![];
        std::io::Read::by_ref(&mut input).take(text_value::PART_BYTES as u64).read_to_end(&mut bytes)?;
        if bytes.is_empty() { break; }
        let reference = object_reference(&bytes);
        if let Some(target) = target {
            target.execute("INSERT OR IGNORE INTO section_objects(hash,bytes) VALUES(?1,?2)", params![hex::encode(reference.content_sha256), bytes])?;
        }
        parts.push(reference);
    }
    Ok(SectionValueRow::PluginBinary { object, parts })
}

pub(super) fn capture<R: Read>(target: Option<&Connection>, space: &str, mut source: impl FnMut() -> StoreResult<R>) -> StoreResult<SectionValueRow> {
    if space == "json" {
        serde_json::from_reader::<_, serde::de::IgnoredAny>(std::io::BufReader::new(source()?))
            .map_err(|_| invalid("Section value object is not JSON"))?;
    }
    let object = match target {
        Some(target) => text_value::store_encoded(target, &mut source)?,
        None => text_value::encode(source()?, io::sink(), text_value::MAX_VALUE_BYTES)?,
    };
    let mut parts = vec![];
    let mut buffer = Vec::with_capacity(text_value::PART_BYTES);
    let mut flush = |buffer: &mut Vec<u8>| -> io::Result<()> {
        if buffer.is_empty() { return Ok(()); }
        let reference = object_reference(buffer);
        if let Some(target) = target {
            target.execute("INSERT OR IGNORE INTO section_objects(hash,bytes) VALUES(?1,?2)",
                params![hex::encode(reference.content_sha256), &*buffer]).map_err(io::Error::other)?;
        }
        parts.push(reference); buffer.clear(); Ok(())
    };
    text_value::visit_text(source()?, |text| {
        let mut remaining = text.as_bytes();
        while !remaining.is_empty() {
            let mut length = remaining.len().min(text_value::PART_BYTES - buffer.len());
            while length > 0 && length < remaining.len() && remaining[length] & 0xc0 == 0x80 { length -= 1; }
            if length == 0 { flush(&mut buffer)?; continue; }
            buffer.extend_from_slice(&remaining[..length]);
            remaining = &remaining[length..];
            if buffer.len() == text_value::PART_BYTES { flush(&mut buffer)?; }
        }
        Ok(())
    })?;
    flush(&mut buffer)?;
    Ok(SectionValueRow::PluginObject { space: space.into(), object, parts })
}

impl SectionSpoolBuilder {
    pub(crate) fn push_entry(&mut self, entry: SectionEntry, object: impl FnMut(&ObjectReference) -> StoreResult<Vec<u8>>) -> StoreResult<()> {
        if !self.versioned { return self.push_backup_entry(entry, object); }
        if entry.kind != self.kind || entry.version.is_none() { return Err(invalid("Section entry has the wrong identity")); }
        let encoded = entry.encode().map_err(section_format_error)?;
        let key = entry.key.clone();
        let hash = risunest_external_storage_format::content_identity::hash(&encoded);
        let row = self.decode_entry(entry, true, object)?;
        self.push(row, &key, &hash)
    }

    pub(super) fn decode_entry(&self, entry: SectionEntry, versioned: bool, mut object: impl FnMut(&ObjectReference) -> StoreResult<Vec<u8>>) -> StoreResult<SectionRow> {
        let (space, parts, binary) = match &entry.value {
            SectionValue::LocalPlugin(LocalPluginValue { space, value: PluginValue::Objects(parts) }) => (space, parts, false),
            SectionValue::LocalPlugin(LocalPluginValue { space, value: PluginValue::Binary(parts) }) => (space, parts, true),
            _ => return SectionRow::from_entry(entry, versioned, object),
        };
        entry.validate().map_err(section_format_error)?;
        let space = match space { PluginSpace::String => "string", PluginSpace::Json => "json" };
        let mut raw = tempfile::tempfile()?;
        for reference in parts {
            let bytes = read_object(reference, &mut object)?;
            raw.write_all(&bytes)?;
            self.connection.execute("INSERT OR IGNORE INTO section_objects(hash,bytes) VALUES(?1,?2)",
                params![hex::encode(reference.content_sha256), bytes])?;
        }
        let source = || { let mut file = raw.try_clone()?; file.seek(SeekFrom::Start(0))?; Ok(file) };
        let mut value = if binary { capture_binary(Some(&self.connection), source)? } else { capture(Some(&self.connection), space, source)? };
        if let SectionValueRow::PluginObject { parts: stored, .. } | SectionValueRow::PluginBinary { parts: stored, .. } = &mut value { *stored = parts.clone(); }
        let (key1, key2, key3) = decode_entry_key(entry.kind, &entry.key)?;
        let (write_clock, writer_id) = match entry.version {
            Some(version) => (version.write_clock, version.writer_id),
            None if !versioned => (Sequence::from(0u64), String::new()),
            None => return Err(invalid("Section entry carries no version")),
        };
        Ok(SectionRow { key1, key2, key3, value, write_clock, writer_id })
    }
}

impl PreparedSectionRows {
    pub(crate) fn retain_plugin_objects(&self, target: &Transaction<'_>) -> StoreResult<()> {
        self.visit(|row| {
            if let SectionValueRow::PluginBinary { object, .. } = &row.value {
                plugin_values::raw_value::copy_object(&self.connection, target, object)?;
            }
            if let SectionValueRow::PluginObject { object, .. } = row.value {
                text_value::copy_object(&self.connection, target, &object)?;
            }
            Ok(())
        })
    }
}
