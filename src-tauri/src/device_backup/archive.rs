//! Adapter between the maintenance spool and the native verified archive.
use super::*;
use crate::local_backup::CancellationProbe;
use crate::portable_backup::{Catalog, VerifiedArchive};
use sha2::Digest;

/// Verify the complete device catalog before any restore selection is applied.
/// This reads the schema-checked archive connection without creating sessions,
/// spools, temporary files, or changing PRAGMAs on the reader's connection.
pub(crate) fn validate_archive_catalog(
    db: &Connection,
    probe: &dyn CancellationProbe,
) -> Result<()> {
    check_cancelled(probe)?;
    // Stream the preliminary membership check too. A single EXISTS over a
    // valid large catalog would otherwise scan every row before observing cancel.
    let mut membership=db.prepare("SELECT s.section IS NOT NULL AND typeof(r.ordinal)='integer' AND r.ordinal>=-1 FROM device_records r LEFT JOIN device_sections s ON s.section=r.section")?;
    let mut membership_rows = membership.query([])?;
    while let Some(row) = membership_rows.next()? {
        check_cancelled(probe)?;
        require(
            row.get(0)?,
            "Archive device records have an invalid section or ordinal",
        )?;
    }
    let mut statement=db.prepare("SELECT CASE WHEN typeof(section)='text' AND length(CAST(section AS BLOB))<=65547 THEN section END,schema_version,included,complete,present,record_count,CASE WHEN typeof(sha256)='text' AND length(sha256)=64 THEN sha256 END FROM device_sections ORDER BY section")?;
    let mut sections = statement.query([])?;
    let mut section_count = 0usize;
    let mut identifier_bytes = 0usize;
    while let Some(row) = sections.next()? {
        check_cancelled(probe)?;
        section_count += 1;
        require(
            section_count <= 1024,
            "Archive device section count exceeds supported bound",
        )?;
        let section: Option<String> = row.get(0)?;
        let section = section.ok_or_else(|| {
            error(
                "device-metadata-invalid",
                "Archive device section identifier exceeds its bound",
            )
        })?;
        validate_section(&section)?;
        identifier_bytes += section.len();
        require(
            identifier_bytes <= 128 * 1024,
            "Archive device identifiers exceed supported bound",
        )?;
        let version: i64 = row.get(1)?;
        let included: i64 = row.get(2)?;
        let complete: i64 = row.get(3)?;
        let present: i64 = row.get(4)?;
        let records: i64 = row.get(5)?;
        require(
            version == 1
                && matches!(included, 0 | 1)
                && matches!(complete, 0 | 1)
                && matches!(present, 0 | 1)
                && records >= 0,
            "Archive device section flags or count are invalid",
        )?;
        require(
            (included == 1 && present == 1) || records == 0,
            "Excluded or absent device section contains records",
        )?;
        let expected: Option<String> = row.get(6)?;
        let expected = expected.ok_or_else(|| {
            error(
                "device-metadata-invalid",
                "Archive device section digest is invalid",
            )
        })?;
        validate_digest(&expected)?;
        let header_count: i64 = db.query_row(
            "SELECT COUNT(*) FROM device_records WHERE section=?1 AND ordinal=-1",
            [&section],
            |row| row.get(0),
        )?;
        require(
            header_count == 1,
            "Archive device section must have exactly one metadata header",
        )?;
        let metadata:Option<Vec<u8>>=db.query_row("SELECT CASE WHEN typeof(metadata)='text' AND length(CAST(metadata AS BLOB))<=262144 THEN CAST(metadata AS BLOB) END FROM device_records WHERE section=?1 AND ordinal=-1",[&section],|row|row.get(0))?;
        let metadata = metadata.ok_or_else(|| {
            error(
                "device-metadata-invalid",
                "Archive device section metadata exceeds its bound",
            )
        })?;
        require(
            spool::validate_section_metadata(&metadata)? == (present == 1),
            "Archive device presence flag differs from section metadata",
        )?;
        let mut hash = spool::start_section_hash(&metadata);
        let mut record_statement=db.prepare("SELECT ordinal,CASE WHEN typeof(metadata)='text' AND length(CAST(metadata AS BLOB))<=67108864 THEN CAST(metadata AS BLOB) END FROM device_records WHERE section=?1 AND ordinal>=0 ORDER BY ordinal")?;
        let mut record_rows = record_statement.query([&section])?;
        let mut observed = 0i64;
        while let Some(row) = record_rows.next()? {
            check_cancelled(probe)?;
            let ordinal: i64 = row.get(0)?;
            require(
                ordinal == observed && observed < records,
                "Archive device record sequence or count is invalid",
            )?;
            let payload: Option<Vec<u8>> = row.get(1)?;
            let payload = payload.ok_or_else(|| {
                error(
                    "device-metadata-invalid",
                    "Archive device graph exceeds its bound",
                )
            })?;
            spool::validate_json(&payload, MAX_GRAPH_BYTES)?;
            spool::start_record_hash(&mut hash, ordinal as u64, payload.len() as u64);
            for chunk in payload.chunks(MAX_CHUNK_BYTES) {
                check_cancelled(probe)?;
                hash.update(chunk);
            }
            observed += 1;
        }
        require(
            observed == records,
            "Archive device record count differs from its manifest",
        )?;
        require(
            hex::encode(hash.finalize()) == expected,
            "Archive device section digest mismatch",
        )?;
    }
    Ok(())
}

fn check_cancelled(probe: &dyn CancellationProbe) -> Result<()> {
    if probe.is_cancelled() {
        Err(error(
            "device-cancelled",
            "Device catalog verification was cancelled",
        ))
    } else {
        Ok(())
    }
}

fn archive_error(error: crate::portable_backup::Error) -> DeviceBackupError {
    match error {
        crate::portable_backup::Error::Cancelled => {
            super::error("device-cancelled", "Device archive operation was cancelled")
        }
        _ => super::error(
            "device-archive-failed",
            "Device archive verification or copying failed",
        ),
    }
}

impl DeviceBackupState {
    pub(crate) fn export_to_catalog(
        &self,
        id: &str,
        spool: Spool,
        catalog: &Catalog,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        require(
            !probe.is_cancelled(),
            "Device archive operation was cancelled",
        )?;
        self.export_spool(id, spool, &catalog.db, |hash, size, reader| {
            catalog
                .add_reader("device", hash, "{}", reader, size, hash, probe)
                .map_err(archive_error)
        })?;
        require(
            !probe.is_cancelled(),
            "Device archive operation was cancelled",
        )
    }

    pub(crate) fn import_from_archive(
        &self,
        id: &str,
        archive: &VerifiedArchive,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        require(
            !probe.is_cancelled(),
            "Device archive operation was cancelled",
        )?;
        self.import_spool(id, &archive.db, &[], |_, _| Ok(()))?;
        // Keep only one descriptor and one bounded copy buffer resident. The
        // archive has already verified object membership, hashes and offsets.
        let mut statement=archive.db.prepare("SELECT DISTINCT lower(hex(f.object_hash)),o.byte_length FROM files f JOIN objects o ON o.sha256=f.object_hash WHERE f.kind='device' AND f.state='present' ORDER BY f.object_hash")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            require(
                !probe.is_cancelled(),
                "Device archive operation was cancelled",
            )?;
            let hash: String = row.get(0)?;
            let size: i64 = row.get(1)?;
            require(size >= 0, "Device archive object has negative length")?;
            self.import_object(id, &hash, size as u64, |writer| {
                archive
                    .copy_object(&hash, writer, probe)
                    .map_err(archive_error)?;
                Ok(())
            })?;
        }
        self.source_ready(id)
    }
}
