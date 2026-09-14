use super::*;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SectionManifest {
    pub(crate) section_id: String,
    pub(crate) metadata_json: String,
    pub(crate) records: u64,
    pub(crate) sha256: String,
    pub(crate) sealed: bool,
    pub(crate) present: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BlobManifest {
    pub(crate) object_id: String,
    pub(crate) bytes: u64,
    pub(crate) sha256: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RowDescriptor {
    pub(crate) ordinal: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) payload_json: Option<String>,
    pub(crate) sha256: String,
    pub(crate) bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RowPage {
    pub(crate) rows: Vec<RowDescriptor>,
    pub(crate) has_more: bool,
}

fn write_allowed(connection: &Connection, id: &str, spool: Spool) -> Result<Session> {
    let session = session_for(connection, id)?;
    let active: bool =
        connection.query_row("SELECT active FROM sessions WHERE id=?1", [id], |r| {
            r.get(0)
        })?;
    let writable = matches!(
        (session.operation, spool, session.phase.as_str()),
        (Operation::Capture, Spool::Source, "capturing")
            | (Operation::Restore, Spool::Source, "loading-source")
            | (Operation::Restore, Spool::Rollback, "preparing")
    );
    require(
        active && writable,
        "Spool is immutable outside its capture phase",
    )?;
    Ok(session)
}

pub(super) fn validate_json(bytes: &[u8], maximum: usize) -> Result<()> {
    require(
        bytes.len() <= maximum,
        "Device metadata exceeds the explicit size limit",
    )?;
    // The clone profile encodes every UTF-16 string in ASCII. JSON text itself
    // must be valid UTF-8; no lossy conversion ever touches stored strings.
    let _: serde_json::Value = serde_json::from_slice(bytes).map_err(|_| {
        error(
            "device-metadata-invalid",
            "Device metadata is not valid JSON",
        )
    })?;
    Ok(())
}

fn update_hash(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

pub(super) fn start_section_hash(metadata: &[u8]) -> Sha256 {
    let mut hash = Sha256::new();
    hash.update(b"RisuNest-device-section-v1\0");
    update_hash(&mut hash, metadata);
    hash
}

pub(super) fn start_record_hash(hash: &mut Sha256, ordinal: u64, length: u64) {
    hash.update(ordinal.to_le_bytes());
    hash.update(length.to_le_bytes());
}

pub(super) fn validate_section_metadata(bytes: &[u8]) -> Result<bool> {
    require(
        bytes.len() <= MAX_CHUNK_BYTES,
        "Device section metadata exceeds size bound",
    )?;
    let metadata: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| error("device-metadata-invalid", "Section metadata is malformed"))?;
    metadata
        .get("present")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| {
            error(
                "device-metadata-invalid",
                "Section metadata requires a presence flag",
            )
        })
}

/// Digest contract: domain bytes, length-prefixed UTF-8 metadata, then for each
/// ordinal: ordinal(u64 LE), payload-length(u64 LE), payload bytes. JSON is kept
/// byte-exact, so independently verified capture must use canonical codec JSON.
fn section_digest(
    connection: &Connection,
    id: &str,
    spool: Spool,
    section: &str,
    metadata: &str,
) -> Result<(u64, String)> {
    let mut hash = start_section_hash(metadata.as_bytes());
    let mut statement=connection.prepare("SELECT ordinal,length(payload),sha256 FROM records WHERE session=?1 AND spool=?2 AND section=?3 ORDER BY ordinal")?;
    let mut rows = statement.query(params![id, spool.key(), section])?;
    let mut count = 0u64;
    while let Some(row) = rows.next()? {
        let ordinal: i64 = row.get(0)?;
        let length: i64 = row.get(1)?;
        let recorded_hash: String = row.get(2)?;
        require(
            ordinal == count as i64 && length >= 0 && length as usize <= MAX_GRAPH_BYTES,
            "Device row sequence is invalid",
        )?;
        start_record_hash(&mut hash, count, length as u64);
        let mut payload_hash = Sha256::new();
        let mut offset = 0;
        while offset < length {
            let bytes:Vec<u8>=connection.query_row("SELECT substr(payload,?5,?6) FROM records WHERE session=?1 AND spool=?2 AND section=?3 AND ordinal=?4",params![id,spool.key(),section,ordinal,offset+1,MAX_CHUNK_BYTES as i64],|r|r.get(0))?;
            require(!bytes.is_empty(), "Device row ended early")?;
            offset += bytes.len() as i64;
            hash.update(&bytes);
            payload_hash.update(&bytes);
        }
        require(
            hex::encode(payload_hash.finalize()) == recorded_hash,
            "Device row digest mismatch",
        )?;
        count += 1;
    }
    Ok((count, hex::encode(hash.finalize())))
}

pub(super) fn verify_section(
    connection: &Connection,
    id: &str,
    spool: Spool,
    section: &str,
) -> Result<SectionManifest> {
    let manifest = section_manifest(connection, id, spool, section)?;
    require(manifest.sealed, "Device section is incomplete")?;
    let (records, digest) =
        section_digest(connection, id, spool, section, &manifest.metadata_json)?;
    require(
        records == manifest.records && digest == manifest.sha256,
        "Device section digest mismatch",
    )?;
    Ok(manifest)
}

pub(super) fn verify_blobs(connection: &Connection, id: &str, spool: Spool) -> Result<()> {
    let mut statement=connection.prepare("SELECT object_id,bytes,sha256,sealed FROM blobs WHERE session=?1 AND spool=?2 ORDER BY object_id")?;
    let mut rows = statement.query(params![id, spool.key()])?;
    while let Some(row) = rows.next()? {
        let object: String = row.get(0)?;
        let bytes = read_unsigned(row, 1)?;
        let expected: Option<String> = row.get(2)?;
        let sealed: bool = row.get(3)?;
        require(sealed, "Device binary spool is incomplete")?;
        let mut position = 0;
        let mut hash = Sha256::new();
        while position < bytes {
            let chunk = read_blob_range(connection, id, spool, &object, position, MAX_CHUNK_BYTES)?;
            position += chunk.len() as u64;
            hash.update(chunk);
        }
        require(
            expected.as_deref() == Some(hex::encode(hash.finalize()).as_str()),
            "Device binary spool digest mismatch",
        )?;
    }
    Ok(())
}

fn section_manifest(
    connection: &Connection,
    id: &str,
    spool: Spool,
    section: &str,
) -> Result<SectionManifest> {
    let (metadata_json,records,sha256,sealed):(String,i64,Option<String>,bool)=connection.query_row("SELECT metadata,records,sha256,sealed FROM sections WHERE session=?1 AND spool=?2 AND section=?3",params![id,spool.key(),section],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?.ok_or_else(||error("device-section-missing","Device section is absent"))?;
    require(records >= 0, "Device section record count is negative")?;
    let present = validate_section_metadata(metadata_json.as_bytes())?;
    Ok(SectionManifest {
        section_id: section.into(),
        metadata_json,
        records: records as u64,
        sha256: sha256.unwrap_or_default(),
        sealed,
        present,
    })
}

fn resolve_blob(
    connection: &Connection,
    id: &str,
    spool: Spool,
    object: &str,
) -> Result<(String, u64, Option<String>, bool)> {
    validate_id(object)?;
    connection.query_row("SELECT object_id,bytes,sha256,sealed FROM blobs WHERE session=?1 AND spool=?2 AND (object_id=?3 OR (sha256=?3 AND sealed=1)) ORDER BY object_id LIMIT 1",params![id,spool.key(),object],|r|Ok((r.get(0)?,read_unsigned(r,1)?,r.get(2)?,r.get(3)?))).optional()?.ok_or_else(||error("device-blob-missing","Device binary object is absent"))
}

fn read_blob_range(
    connection: &Connection,
    id: &str,
    spool: Spool,
    object: &str,
    offset: u64,
    length: usize,
) -> Result<Vec<u8>> {
    require(length <= MAX_CHUNK_BYTES, "Binary read exceeds chunk bound")?;
    let (object, bytes, _, sealed) = resolve_blob(connection, id, spool, object)?;
    require(
        sealed && offset <= bytes,
        "Binary object is incomplete or offset is invalid",
    )?;
    let end = offset.saturating_add(length as u64).min(bytes);
    let mut statement=connection.prepare("SELECT offset,bytes FROM chunks WHERE session=?1 AND spool=?2 AND object_id=?3 AND offset<?5 AND offset+length(bytes)>?4 ORDER BY offset")?;
    let mut rows = statement.query(params![
        id,
        spool.key(),
        object,
        sql_integer(offset)?,
        sql_integer(end)?
    ])?;
    let mut output = Vec::with_capacity(length.min((bytes - offset) as usize));
    let mut position = offset;
    while let Some(row) = rows.next()? {
        let start = read_unsigned(row, 0)?;
        let chunk: Vec<u8> = row.get(1)?;
        require(
            start <= position && start + chunk.len() as u64 > position,
            "Binary object has a gap",
        )?;
        let begin = (position - start) as usize;
        let take = ((end - position) as usize).min(chunk.len() - begin);
        output.extend_from_slice(&chunk[begin..begin + take]);
        position += take as u64;
    }
    require(position == end, "Binary object ended early")?;
    Ok(output)
}

impl DeviceBackupState {
    pub(crate) fn section_begin(
        &self,
        id: &str,
        spool: Spool,
        section: &str,
        metadata_json: &str,
    ) -> Result<()> {
        validate_section(section)?;
        validate_section_metadata(metadata_json.as_bytes())?;
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = write_allowed(connection, id, spool)?;
        require(
            session.selected_sections.iter().any(|s| s == section),
            "Section is not selected",
        )?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "DELETE FROM sections WHERE session=?1 AND spool=?2 AND section=?3",
            params![id, spool.key(), section],
        )?;
        transaction.execute(
            "INSERT INTO sections(session,spool,section,metadata) VALUES(?1,?2,?3,?4)",
            params![id, spool.key(), section, metadata_json],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn row_append(
        &self,
        id: &str,
        spool: Spool,
        section: &str,
        ordinal: u64,
        payload_json: &str,
    ) -> Result<()> {
        validate_json(payload_json.as_bytes(), MAX_CHUNK_BYTES)?;
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        append_row(
            connection,
            id,
            spool,
            section,
            ordinal,
            payload_json.as_bytes(),
        )
    }

    pub(crate) fn row_append_from_blob(
        &self,
        id: &str,
        spool: Spool,
        section: &str,
        ordinal: u64,
        sha256: &str,
    ) -> Result<()> {
        validate_digest(sha256)?;
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        write_allowed(connection, id, spool)?;
        let (object, length, digest, sealed) = resolve_blob(connection, id, spool, sha256)?;
        require(
            sealed && digest.as_deref() == Some(sha256) && length <= MAX_GRAPH_BYTES as u64,
            "Metadata binary exceeds graph bound or is incomplete",
        )?;
        let mut payload = Vec::with_capacity(length as usize);
        let mut hash = Sha256::new();
        while payload.len() < length as usize {
            let bytes = read_blob_range(
                connection,
                id,
                spool,
                sha256,
                payload.len() as u64,
                MAX_CHUNK_BYTES,
            )?;
            hash.update(&bytes);
            payload.extend_from_slice(&bytes);
        }
        require(
            hex::encode(hash.finalize()) == sha256,
            "Metadata binary digest mismatch",
        )?;
        validate_json(&payload, MAX_GRAPH_BYTES)?;
        append_row(connection, id, spool, section, ordinal, &payload)?;
        connection.execute(
            "UPDATE blobs SET metadata_only=1 WHERE session=?1 AND spool=?2 AND object_id=?3",
            params![id, spool.key(), object],
        )?;
        Ok(())
    }

    pub(crate) fn section_finish(
        &self,
        id: &str,
        spool: Spool,
        section: &str,
    ) -> Result<SectionManifest> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        write_allowed(connection, id, spool)?;
        let manifest = section_manifest(connection, id, spool, section)?;
        if manifest.sealed {
            return verify_section(connection, id, spool, section);
        };
        let unfinished: i64 = connection.query_row(
            "SELECT COUNT(*) FROM blobs WHERE session=?1 AND spool=?2 AND sealed=0",
            params![id, spool.key()],
            |r| r.get(0),
        )?;
        require(unfinished == 0, "Device binaries are still incomplete")?;
        let (records, digest) =
            section_digest(connection, id, spool, section, &manifest.metadata_json)?;
        require(
            manifest.present || records == 0,
            "Absent device section contains records",
        )?;
        connection.execute("UPDATE sections SET sealed=1,records=?4,sha256=?5 WHERE session=?1 AND spool=?2 AND section=?3",params![id,spool.key(),section,sql_integer(records)?,digest])?;
        section_manifest(connection, id, spool, section)
    }

    pub(crate) fn section_list(&self, id: &str, spool: Spool) -> Result<Vec<SectionManifest>> {
        // Native-only convenience for bounded inventory consumption.
        let mut output = Vec::new();
        let mut after = None;
        loop {
            let page = self.section_page(id, spool, after.as_deref(), 128)?;
            if page.is_empty() {
                break;
            }
            after = page.last().map(|m| m.section_id.clone());
            output.extend(page);
        }
        Ok(output)
    }

    pub(crate) fn section_page(
        &self,
        id: &str,
        spool: Spool,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SectionManifest>> {
        require((1..=128).contains(&limit), "Invalid section page limit")?;
        let inner = self.lock()?;
        let connection = inner.connection.as_ref().unwrap();
        let mut session = session_for(connection, id)?;
        session.selected_sections.sort();
        let mut result = Vec::new();
        let mut budget = MAX_CHUNK_BYTES;
        for section in session.selected_sections {
            if after.is_some_and(|after| section.as_str() <= after) {
                continue;
            }
            let exists:bool=connection.query_row("SELECT EXISTS(SELECT 1 FROM sections WHERE session=?1 AND spool=?2 AND section=?3)",params![id,spool.key(),section],|r|r.get(0))?;
            if exists {
                let manifest = section_manifest(connection, id, spool, &section)?;
                let cost = manifest.metadata_json.len() + manifest.section_id.len() + 256;
                if !result.is_empty() && cost > budget {
                    break;
                }
                budget = budget.saturating_sub(cost);
                result.push(manifest);
                if result.len() == limit as usize {
                    break;
                }
            }
        }
        Ok(result)
    }

    pub(crate) fn row_read(
        &self,
        id: &str,
        spool: Spool,
        section: &str,
        after_ordinal: Option<u64>,
        limit: u32,
    ) -> Result<RowPage> {
        require((1..=128).contains(&limit), "Invalid device row page limit")?;
        let after = after_ordinal
            .map(i64::try_from)
            .transpose()
            .map_err(|_| error("device-invalid-state", "Row ordinal exceeds range"))?
            .unwrap_or(-1);
        let inner = self.lock()?;
        let connection = inner.connection.as_ref().unwrap();
        session_for(connection, id)?;
        let mut statement=connection.prepare("SELECT ordinal,length(payload),sha256 FROM records WHERE session=?1 AND spool=?2 AND section=?3 AND ordinal>?4 ORDER BY ordinal LIMIT ?5")?;
        let mut cursor =
            statement.query(params![id, spool.key(), section, after, limit as i64 + 1])?;
        let mut rows = Vec::new();
        let mut budget = MAX_CHUNK_BYTES;
        let mut has_more = false;
        while let Some(row) = cursor.next()? {
            if rows.len() == limit as usize {
                has_more = true;
                break;
            }
            let ordinal = read_unsigned(row, 0)?;
            let bytes = read_unsigned(row, 1)?;
            let sha256: String = row.get(2)?;
            let payload_json = if bytes <= budget as u64 {
                let payload:Vec<u8>=connection.query_row("SELECT payload FROM records WHERE session=?1 AND spool=?2 AND section=?3 AND ordinal=?4",params![id,spool.key(),section,sql_integer(ordinal)?],|r|r.get(0))?;
                budget -= payload.len();
                Some(
                    String::from_utf8(payload).map_err(|_| {
                        error("device-metadata-invalid", "Device row UTF-8 is invalid")
                    })?,
                )
            } else {
                None
            };
            rows.push(RowDescriptor {
                ordinal,
                payload_json,
                sha256,
                bytes,
            });
        }
        Ok(RowPage { rows, has_more })
    }

    pub(crate) fn row_read_bytes(
        &self,
        id: &str,
        spool: Spool,
        section: &str,
        ordinal: u64,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        require(
            length <= MAX_CHUNK_BYTES && offset <= MAX_GRAPH_BYTES as u64,
            "Row range exceeds bound",
        )?;
        let inner = self.lock()?;
        let connection = inner.connection.as_ref().unwrap();
        session_for(connection, id)?;
        let (total,bytes):(u64,Vec<u8>)=connection.query_row("SELECT length(payload),substr(payload,?5,?6) FROM records WHERE session=?1 AND spool=?2 AND section=?3 AND ordinal=?4",params![id,spool.key(),section,sql_integer(ordinal)?,sql_integer(offset+1)?,length as i64],|r|Ok((read_unsigned(r,0)?,r.get(1)?)))?;
        require(offset <= total, "Row offset is beyond its end")?;
        Ok(bytes)
    }

    pub(crate) fn blob_begin(&self, id: &str, spool: Spool, object_id: &str) -> Result<()> {
        validate_id(object_id)?;
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        write_allowed(connection, id, spool)?;
        connection.execute(
            "INSERT INTO blobs(session,spool,object_id) VALUES(?1,?2,?3)",
            params![id, spool.key(), object_id],
        )?;
        Ok(())
    }

    pub(crate) fn blob_append(
        &self,
        id: &str,
        spool: Spool,
        object_id: &str,
        offset: u64,
        bytes: &[u8],
    ) -> Result<()> {
        require(
            !bytes.is_empty() && bytes.len() <= MAX_CHUNK_BYTES,
            "Binary append exceeds chunk bound",
        )?;
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        write_allowed(connection, id, spool)?;
        let (object, current, _, sealed) = resolve_blob(connection, id, spool, object_id)?;
        require(
            !sealed
                && current == offset
                && offset
                    .checked_add(bytes.len() as u64)
                    .is_some_and(|v| v <= i64::MAX as u64),
            "Binary append is out of sequence",
        )?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "INSERT INTO chunks(session,spool,object_id,offset,bytes) VALUES(?1,?2,?3,?4,?5)",
            params![id, spool.key(), object, sql_integer(offset)?, bytes],
        )?;
        transaction.execute(
            "UPDATE blobs SET bytes=?4 WHERE session=?1 AND spool=?2 AND object_id=?3",
            params![
                id,
                spool.key(),
                object,
                sql_integer(offset + bytes.len() as u64)?
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn blob_finish(
        &self,
        id: &str,
        spool: Spool,
        object_id: &str,
    ) -> Result<BlobManifest> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        write_allowed(connection, id, spool)?;
        let (object, expected, existing, sealed) = resolve_blob(connection, id, spool, object_id)?;
        let mut statement=connection.prepare("SELECT offset,bytes FROM chunks WHERE session=?1 AND spool=?2 AND object_id=?3 ORDER BY offset")?;
        let mut rows = statement.query(params![id, spool.key(), object])?;
        let mut position = 0u64;
        let mut hash = Sha256::new();
        while let Some(row) = rows.next()? {
            let offset = read_unsigned(row, 0)?;
            let bytes: Vec<u8> = row.get(1)?;
            require(
                offset == position && !bytes.is_empty() && bytes.len() <= MAX_CHUNK_BYTES,
                "Binary chunk sequence is invalid",
            )?;
            position += bytes.len() as u64;
            hash.update(bytes);
        }
        require(position == expected, "Binary object length mismatch")?;
        let sha256 = hex::encode(hash.finalize());
        require(
            !sealed || existing.as_deref() == Some(&sha256),
            "Sealed binary digest mismatch",
        )?;
        connection.execute(
            "UPDATE blobs SET sealed=1,sha256=?4 WHERE session=?1 AND spool=?2 AND object_id=?3",
            params![id, spool.key(), object, sha256],
        )?;
        Ok(BlobManifest {
            object_id: object,
            bytes: position,
            sha256,
        })
    }

    pub(crate) fn blob_read(
        &self,
        id: &str,
        spool: Spool,
        object_id: &str,
        offset: u64,
        length: usize,
    ) -> Result<Vec<u8>> {
        let inner = self.lock()?;
        let connection = inner.connection.as_ref().unwrap();
        session_for(connection, id)?;
        read_blob_range(connection, id, spool, object_id, offset, length)
    }

    /// Native archive boundary. The returned metadata rows include ordinal -1
    /// for the section header. Value rows remain individually addressable.
    pub(crate) fn export_spool(
        &self,
        id: &str,
        spool: Spool,
        destination: &Connection,
        mut object_sink: impl FnMut(&str, u64, &mut dyn Read) -> Result<()>,
    ) -> Result<()> {
        let objects = {
            let inner = self.lock()?;
            let connection = inner.connection.as_ref().unwrap();
            let session = session_for(connection, id)?;
            for section in &session.selected_sections {
                let manifest = verify_section(connection, id, spool, section)?;
                destination.execute("INSERT INTO device_sections(section,schema_version,included,complete,present,record_count,sha256) VALUES(?1,1,1,1,?2,?3,?4)",params![section,manifest.present,sql_integer(manifest.records)?,manifest.sha256])?;
                destination.execute(
                    "INSERT INTO device_records(section,ordinal,metadata) VALUES(?1,-1,?2)",
                    params![section, manifest.metadata_json],
                )?;
                let mut statement=connection.prepare("SELECT ordinal,payload FROM records WHERE session=?1 AND spool=?2 AND section=?3 ORDER BY ordinal")?;
                let mut rows = statement.query(params![id, spool.key(), section])?;
                while let Some(row) = rows.next()? {
                    let ordinal: i64 = row.get(0)?;
                    let payload: Vec<u8> = row.get(1)?;
                    let text = std::str::from_utf8(&payload).map_err(|_| {
                        error("device-metadata-invalid", "Device row UTF-8 is invalid")
                    })?;
                    destination.execute(
                        "INSERT INTO device_records(section,ordinal,metadata) VALUES(?1,?2,?3)",
                        params![section, ordinal, text],
                    )?;
                }
            }
            let mut statement=connection.prepare("SELECT sha256,MAX(bytes) FROM blobs WHERE session=?1 AND spool=?2 AND sealed=1 AND metadata_only=0 GROUP BY sha256")?;
            let mut rows = statement.query(params![id, spool.key()])?;
            let mut objects = Vec::new();
            while let Some(row) = rows.next()? {
                objects.push((row.get::<_, String>(0)?, read_unsigned(row, 1)?));
            }
            objects
        };
        for (hash, bytes) in objects {
            let mut reader = BlobReader {
                state: self,
                id,
                spool,
                object: &hash,
                position: 0,
                length: bytes,
            };
            object_sink(&hash, bytes, &mut reader)?;
            require(
                reader.position == bytes,
                "Archive did not consume complete device binary",
            )?;
        }
        Ok(())
    }

    /// Source is a native-verified archive connection, never an IPC path. Blob
    /// hashes are supplied from the verified archive's device object inventory.
    pub(crate) fn import_spool(
        &self,
        id: &str,
        source: &Connection,
        objects: &[(String, u64)],
        mut object_source: impl FnMut(&str, &mut dyn Write) -> Result<()>,
    ) -> Result<()> {
        let session = self.session(id)?;
        require(
            session.operation == Operation::Restore && session.phase == "loading-source",
            "Source import has wrong phase",
        )?;
        for section in &session.selected_sections {
            let (version,included,complete,present,count,hash):(i64,bool,bool,bool,u64,String)=source.query_row("SELECT schema_version,included,complete,present,record_count,sha256 FROM device_sections WHERE section=?1",[section],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,read_unsigned(r,4)?,r.get(5)?)))?;
            require(
                version == 1 && included && complete,
                "Archive device section is unsupported or incomplete",
            )?;
            validate_digest(&hash)?;
            let metadata: Option<String> = source.query_row(
                "SELECT CASE WHEN length(CAST(metadata AS BLOB))<=262144 THEN metadata ELSE NULL END FROM device_records WHERE section=?1 AND ordinal=-1",
                [section],
                |r| r.get(0),
            )?;
            let metadata = metadata.ok_or_else(|| {
                error(
                    "device-metadata-invalid",
                    "Archive section metadata exceeds size bound",
                )
            })?;
            self.section_begin(id, Spool::Source, section, &metadata)?;
            let mut statement=source.prepare("SELECT ordinal,CASE WHEN length(CAST(metadata AS BLOB))<=67108864 THEN metadata ELSE NULL END FROM device_records WHERE section=?1 AND ordinal>=0 ORDER BY ordinal")?;
            let mut rows = statement.query([section])?;
            let mut found = 0;
            while let Some(row) = rows.next()? {
                let ordinal = read_unsigned(row, 0)?;
                let metadata: Option<String> = row.get(1)?;
                let metadata = metadata.ok_or_else(|| {
                    error(
                        "device-metadata-invalid",
                        "Archive device graph exceeds size bound",
                    )
                })?;
                validate_json(metadata.as_bytes(), MAX_GRAPH_BYTES)?;
                let mut inner = self.lock()?;
                append_row(
                    inner.connection.as_mut().unwrap(),
                    id,
                    Spool::Source,
                    section,
                    ordinal,
                    metadata.as_bytes(),
                )?;
                found += 1;
            }
            let manifest = self.section_finish(id, Spool::Source, section)?;
            require(
                found == count && manifest.sha256 == hash && manifest.present == present,
                "Archive device section verification failed",
            )?;
        }
        for (hash, length) in objects {
            self.import_object(id, hash, *length, |writer| object_source(hash, writer))?;
        }
        Ok(())
    }

    pub(super) fn import_object(
        &self,
        id: &str,
        hash: &str,
        length: u64,
        source: impl FnOnce(&mut dyn Write) -> Result<()>,
    ) -> Result<()> {
        validate_digest(hash)?;
        self.blob_begin(id, Spool::Source, hash)?;
        let mut writer = BlobWriter {
            state: self,
            id,
            hash,
            position: 0,
            expected: length,
        };
        source(&mut writer)?;
        require(
            writer.position == length,
            "Archive device binary length mismatch",
        )?;
        let manifest = self.blob_finish(id, Spool::Source, hash)?;
        require(
            manifest.sha256 == hash,
            "Archive device binary digest mismatch",
        )
    }
}

fn append_row(
    connection: &mut Connection,
    id: &str,
    spool: Spool,
    section: &str,
    ordinal: u64,
    payload: &[u8],
) -> Result<()> {
    write_allowed(connection, id, spool)?;
    let manifest = section_manifest(connection, id, spool, section)?;
    require(
        !manifest.sealed && ordinal == manifest.records,
        "Device row append is out of sequence or section is sealed",
    )?;
    let hash = hex::encode(Sha256::digest(payload));
    let transaction = connection.transaction()?;
    transaction.execute("INSERT INTO records(session,spool,section,ordinal,payload,sha256) VALUES(?1,?2,?3,?4,?5,?6)",params![id,spool.key(),section,sql_integer(ordinal)?,payload,hash])?;
    transaction.execute(
        "UPDATE sections SET records=records+1 WHERE session=?1 AND spool=?2 AND section=?3",
        params![id, spool.key(), section],
    )?;
    transaction.commit()?;
    Ok(())
}

struct BlobReader<'a> {
    state: &'a DeviceBackupState,
    id: &'a str,
    spool: Spool,
    object: &'a str,
    position: u64,
    length: u64,
}
impl Read for BlobReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.position == self.length || buffer.is_empty() {
            return Ok(0);
        }
        let bytes = self
            .state
            .blob_read(
                self.id,
                self.spool,
                self.object,
                self.position,
                buffer.len().min(MAX_CHUNK_BYTES),
            )
            .map_err(std::io::Error::other)?;
        buffer[..bytes.len()].copy_from_slice(&bytes);
        self.position += bytes.len() as u64;
        Ok(bytes.len())
    }
}
struct BlobWriter<'a> {
    state: &'a DeviceBackupState,
    id: &'a str,
    hash: &'a str,
    position: u64,
    expected: u64,
}
impl Write for BlobWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let length = bytes.len().min(MAX_CHUNK_BYTES);
        if self.position.saturating_add(length as u64) > self.expected {
            return Err(std::io::Error::other(
                "Device binary exceeds declared length",
            ));
        }
        self.state
            .blob_append(
                self.id,
                Spool::Source,
                self.hash,
                self.position,
                &bytes[..length],
            )
            .map_err(std::io::Error::other)?;
        self.position += length as u64;
        Ok(length)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn sql_integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| {
        error(
            "device-invalid-state",
            "Device size exceeds SQLite integer range",
        )
    })
}
fn read_unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}
