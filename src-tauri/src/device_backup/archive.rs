//! Adapter between the maintenance spool and the native verified archive.
use super::*;
use crate::local_backup::CancellationProbe;
use crate::persistent_store::{
    device_store::sections::{PreparedSectionRows, SectionSpoolBuilder},
    PersistentStore, StoreError,
};
use crate::portable_backup::{Catalog, VerifiedArchive};
use risunest_external_storage_format::{
    content_identity::hash as content_hash,
    format::FingerprintBuilder,
    section::{
        InlineOrObject, ObjectReference, SectionEntry, SectionKind, SectionValue,
        MAX_SECTION_ENTRY_BYTES, SECTION_CODEC,
    },
};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Cursor;

const NATIVE_SECTION_PROFILE: &str = "risunest.native-device-section/v1";

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NativeSectionHeader {
    profile: String,
    codec: String,
    kind: SectionKind,
    content_fingerprint: String,
    present: bool,
}

pub(crate) enum PreparedDeviceSection {
    Hypa(PreparedSectionRows),
    LocalPlugins(PreparedSectionRows),
    LocalSettings(PreparedSectionRows),
}

impl PreparedDeviceSection {
    fn rows(&self) -> &PreparedSectionRows {
        match self {
            Self::Hypa(rows) | Self::LocalPlugins(rows) | Self::LocalSettings(rows) => rows,
        }
    }
}

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
        let kind = SectionKind::parse(&section).map_err(section_codec_error)?;
        require(
            included == 1 && complete == 1 && present == 1,
            "Native backup sections must be included, complete, and present",
        )?;
        let native_header = parse_native_header(&metadata, kind)?;
        let mut fingerprint = FingerprintBuilder::new(&kind.fingerprint_domain());
        let mut transport_hash = spool::start_section_hash(&metadata);
        let record_limit = MAX_SECTION_ENTRY_BYTES;
        let mut record_statement=db.prepare("SELECT ordinal,CASE WHEN typeof(metadata)='text' AND length(CAST(metadata AS BLOB))<=?2 THEN CAST(metadata AS BLOB) END FROM device_records WHERE section=?1 AND ordinal>=0 ORDER BY ordinal")?;
        let mut record_rows = record_statement.query(params![&section, record_limit as i64])?;
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
            let entry = SectionEntry::decode(&payload).map_err(section_codec_error)?;
            require(
                entry.kind == kind && entry.version.is_none(),
                "Native backup entry has the wrong section identity",
            )?;
            require(
                entry.encode().map_err(section_codec_error)? == payload,
                "Native backup entry is not canonically encoded",
            )?;
            fingerprint
                .push(&entry.key, &content_hash(&payload))
                .map_err(section_codec_error)?;
            if let Some(reference) = object_reference(&entry) {
                validate_native_object_reference(db, kind, reference)?;
            }
            spool::start_record_hash(&mut transport_hash, ordinal as u64, payload.len() as u64);
            for chunk in payload.chunks(MAX_CHUNK_BYTES) {
                check_cancelled(probe)?;
                transport_hash.update(chunk);
            }
            observed += 1;
        }
        require(
            observed == records,
            "Archive device record count differs from its manifest",
        )?;
        require(
            hex::encode(transport_hash.finalize()) == expected,
            "Archive device section digest mismatch",
        )?;
        require(
            hex::encode(fingerprint.finish()) == native_header.content_fingerprint,
            "Native backup section fingerprint mismatch",
        )?;
    }
    Ok(())
}

fn parse_native_header(metadata: &[u8], kind: SectionKind) -> Result<NativeSectionHeader> {
    let header: NativeSectionHeader = serde_json::from_slice(metadata).map_err(|_| {
        error(
            "device-metadata-invalid",
            "Native backup section header is malformed",
        )
    })?;
    require(
        header.profile == NATIVE_SECTION_PROFILE
            && header.codec == SECTION_CODEC
            && header.kind == kind
            && header.present
            && crate::trust_boundary::is_lower_hex_256(&header.content_fingerprint),
        "Native backup section header is invalid",
    )?;
    Ok(header)
}

fn object_reference(entry: &SectionEntry) -> Option<&ObjectReference> {
    match &entry.value {
        SectionValue::Hypa(value) => match &value.vector {
            InlineOrObject::Object(reference) => Some(reference),
            InlineOrObject::Inline(_) => None,
        },
        _ => None,
    }
}

fn native_object_key(kind: SectionKind, reference: &ObjectReference) -> String {
    format!(
        "section/{}/object/{}",
        kind.id(),
        hex::encode(reference.content_sha256)
    )
}

fn validate_native_object_reference(
    db: &Connection,
    kind: SectionKind,
    reference: &ObjectReference,
) -> Result<()> {
    let digest = reference.content_sha256.as_slice();
    let count: i64 = db.query_row(
        "SELECT COUNT(*) FROM files f JOIN objects o ON o.sha256=f.object_hash
            WHERE f.kind='device' AND f.logical_key=?1 AND f.object_hash=?2
              AND f.expected_hash=?3 AND f.state='present' AND o.byte_length=?4",
        params![
            native_object_key(kind, reference),
            digest,
            hex::encode(reference.content_sha256),
            i64::try_from(reference.byte_length).map_err(|_| error(
                "device-metadata-invalid",
                "Native backup object length exceeds the archive range",
            ))?,
        ],
        |row| row.get(0),
    )?;
    require(
        count == 1,
        "Native backup object reference has no matching archive object",
    )
}

fn section_codec_error(_: risunest_external_storage_format::FormatError) -> DeviceBackupError {
    error(
        "device-metadata-invalid",
        "Native backup section codec is invalid",
    )
}

fn device_store_error(failure: StoreError) -> DeviceBackupError {
    match failure {
        StoreError::Validation { .. } => error(
            "device-metadata-invalid",
            "Native backup section material is invalid",
        ),
        _ => error(
            "device-storage-failed",
            "Native backup section storage failed",
        ),
    }
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

fn selected_native_kinds(section_ids: &[String]) -> Result<Vec<SectionKind>> {
    require(
        section_ids.len() <= 3,
        "Native backup selects too many logical sections",
    )?;
    let mut seen = BTreeSet::new();
    let mut kinds = Vec::with_capacity(section_ids.len());
    for section_id in section_ids {
        let kind = SectionKind::parse(section_id).map_err(section_codec_error)?;
        require(seen.insert(kind), "Native backup repeats a logical section")?;
        kinds.push(kind);
    }
    Ok(kinds)
}

pub(crate) fn capture_native_sections(
    store: &mut PersistentStore,
    section_ids: &[String],
    catalog: &Catalog,
    probe: &dyn CancellationProbe,
) -> Result<()> {
    let sections = capture_prepared_native_sections(store, section_ids, probe)?;
    for section in &sections {
        write_native_section(catalog, section.rows(), probe)?;
    }
    Ok(())
}

pub(crate) fn capture_prepared_native_sections(
    store: &mut PersistentStore,
    section_ids: &[String],
    probe: &dyn CancellationProbe,
) -> Result<Vec<PreparedDeviceSection>> {
    let kinds = selected_native_kinds(section_ids)?;
    check_cancelled(probe)?;
    let sections = store
        .device_store_mut()
        .map_err(device_store_error)?
        .capture_backup_sections(&kinds)
        .map_err(device_store_error)?;
    require(
        sections.len() == kinds.len(),
        "Native backup capture omitted a selected section",
    )?;
    kinds
        .into_iter()
        .zip(sections)
        .map(|(expected, rows)| {
            require(
                rows.kind() == expected,
                "Native backup capture returned sections out of order",
            )?;
            Ok(prepared_section(rows))
        })
        .collect()
}

fn prepared_section(rows: PreparedSectionRows) -> PreparedDeviceSection {
    match rows.kind() {
        SectionKind::Hypa => PreparedDeviceSection::Hypa(rows),
        SectionKind::LocalPlugins => PreparedDeviceSection::LocalPlugins(rows),
        SectionKind::LocalSettings => PreparedDeviceSection::LocalSettings(rows),
    }
}

fn write_native_section(
    catalog: &Catalog,
    section: &PreparedSectionRows,
    probe: &dyn CancellationProbe,
) -> Result<()> {
    let kind = section.kind();
    let mut ordinal = 0u64;
    let mut fingerprint = FingerprintBuilder::new(&kind.fingerprint_domain());
    let mut object_lengths = BTreeMap::<String, u64>::new();
    let mut callback_failure = None;
    let visited = section.visit_entries(|entry, object| {
        if callback_failure.is_some() {
            return Err(StoreError::Validation {
                message: "Native archive callback already failed".into(),
            });
        }
        let result = (|| -> Result<()> {
            check_cancelled(probe)?;
            let payload = entry.encode().map_err(section_codec_error)?;
            fingerprint
                .push(&entry.key, &content_hash(&payload))
                .map_err(section_codec_error)?;
            let payload = String::from_utf8(payload).map_err(|_| {
                error(
                    "device-metadata-invalid",
                    "Native backup entry is not UTF-8",
                )
            })?;
            catalog.db.execute(
                "INSERT INTO device_records(section,ordinal,metadata) VALUES(?1,?2,?3)",
                params![
                    kind.id(),
                    i64::try_from(ordinal).map_err(|_| error(
                        "device-metadata-invalid",
                        "Native backup entry count exceeds the archive range",
                    ))?,
                    payload,
                ],
            )?;
            ordinal = ordinal.checked_add(1).ok_or_else(|| {
                error(
                    "device-metadata-invalid",
                    "Native backup entry count is exhausted",
                )
            })?;
            match (object_reference(entry), object) {
                (None, None) => {}
                (Some(reference), Some(bytes)) => {
                    require(
                        reference.byte_length == bytes.len() as u64
                            && reference.content_sha256 == content_hash(bytes),
                        "Native backup object differs from its section entry",
                    )?;
                    let digest = hex::encode(reference.content_sha256);
                    match object_lengths.insert(digest.clone(), reference.byte_length) {
                        Some(length) => require(
                            length == reference.byte_length,
                            "Native backup object hash has conflicting lengths",
                        )?,
                        None => {
                            let mut reader = Cursor::new(bytes);
                            catalog
                                .add_reader(
                                    "device",
                                    &native_object_key(kind, reference),
                                    "{}",
                                    &mut reader,
                                    reference.byte_length,
                                    &digest,
                                    probe,
                                )
                                .map_err(archive_error)?;
                        }
                    }
                }
                _ => {
                    return Err(error(
                        "device-metadata-invalid",
                        "Native backup object presence differs from its section entry",
                    ))
                }
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(()),
            Err(failure) => {
                callback_failure = Some(failure);
                Err(StoreError::Validation {
                    message: "Native archive callback failed".into(),
                })
            }
        }
    });
    if let Some(failure) = callback_failure {
        return Err(failure);
    }
    visited.map_err(device_store_error)?;
    require(
        ordinal == section.len(),
        "Native backup capture returned the wrong entry count",
    )?;
    let header = NativeSectionHeader {
        profile: NATIVE_SECTION_PROFILE.into(),
        codec: SECTION_CODEC.into(),
        kind,
        content_fingerprint: hex::encode(fingerprint.finish()),
        present: true,
    };
    let header = serde_json::to_vec(&header).map_err(|_| {
        error(
            "device-metadata-invalid",
            "Native backup section header could not be encoded",
        )
    })?;
    catalog.db.execute(
        "INSERT INTO device_records(section,ordinal,metadata) VALUES(?1,-1,?2)",
        params![
            kind.id(),
            std::str::from_utf8(&header).expect("JSON is UTF-8")
        ],
    )?;
    let transport_digest = native_transport_digest(&catalog.db, kind, &header, probe)?;
    catalog.db.execute(
        "INSERT INTO device_sections(section,schema_version,included,complete,present,record_count,sha256)
            VALUES(?1,1,1,1,1,?2,?3)",
        params![
            kind.id(),
            i64::try_from(ordinal).map_err(|_| error(
                "device-metadata-invalid",
                "Native backup entry count exceeds the archive range",
            ))?,
            transport_digest,
        ],
    )?;
    Ok(())
}

fn native_transport_digest(
    db: &Connection,
    kind: SectionKind,
    header: &[u8],
    probe: &dyn CancellationProbe,
) -> Result<String> {
    let mut transport_hash = spool::start_section_hash(header);
    let mut statement = db.prepare(
        "SELECT ordinal,CAST(metadata AS BLOB) FROM device_records
            WHERE section=?1 AND ordinal>=0 ORDER BY ordinal",
    )?;
    let mut rows = statement.query([kind.id()])?;
    let mut expected = 0u64;
    while let Some(row) = rows.next()? {
        check_cancelled(probe)?;
        let ordinal: i64 = row.get(0)?;
        require(
            ordinal >= 0 && ordinal as u64 == expected,
            "Native backup entries are not contiguous",
        )?;
        let payload: Vec<u8> = row.get(1)?;
        spool::start_record_hash(&mut transport_hash, expected, payload.len() as u64);
        for chunk in payload.chunks(MAX_CHUNK_BYTES) {
            check_cancelled(probe)?;
            transport_hash.update(chunk);
        }
        expected += 1;
    }
    Ok(hex::encode(transport_hash.finalize()))
}

pub(crate) fn prepare_native_sections(
    archive: &VerifiedArchive,
    section_ids: &[String],
    probe: &dyn CancellationProbe,
) -> Result<Vec<PreparedDeviceSection>> {
    let kinds = selected_native_kinds(section_ids)?;
    let mut prepared = Vec::with_capacity(kinds.len());
    for kind in kinds {
        check_cancelled(probe)?;
        let (included, complete, present, records, metadata): (bool, bool, bool, i64, Vec<u8>) =
            archive.db.query_row(
                "SELECT s.included,s.complete,s.present,s.record_count,CAST(r.metadata AS BLOB)
                    FROM device_sections s JOIN device_records r
                      ON r.section=s.section AND r.ordinal=-1 WHERE s.section=?1",
                [kind.id()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )?;
        require(
            included && complete && present && records >= 0,
            "Selected native backup section is unavailable",
        )?;
        let records = records as u64;
        let header = parse_native_header(&metadata, kind)?;
        let expected_fingerprint: [u8; 32] = hex::decode(&header.content_fingerprint)
            .map_err(|_| {
                error(
                    "device-metadata-invalid",
                    "Native backup section fingerprint is invalid",
                )
            })?
            .try_into()
            .map_err(|_| {
                error(
                    "device-metadata-invalid",
                    "Native backup section fingerprint has the wrong length",
                )
            })?;
        let mut builder = SectionSpoolBuilder::new_backup(kind).map_err(device_store_error)?;
        let mut statement = archive.db.prepare(
            "SELECT ordinal,CAST(metadata AS BLOB) FROM device_records
                WHERE section=?1 AND ordinal>=0 ORDER BY ordinal",
        )?;
        let mut rows = statement.query([kind.id()])?;
        let mut observed = 0u64;
        while let Some(row) = rows.next()? {
            check_cancelled(probe)?;
            let ordinal: i64 = row.get(0)?;
            require(
                ordinal >= 0 && ordinal as u64 == observed,
                "Native backup entries are not contiguous",
            )?;
            let payload: Vec<u8> = row.get(1)?;
            let entry = SectionEntry::decode(&payload).map_err(section_codec_error)?;
            let mut object = match object_reference(&entry) {
                Some(reference) => Some(read_native_object(archive, reference, probe)?),
                None => None,
            };
            builder
                .push_backup_entry(entry, |_| {
                    object.take().ok_or_else(|| StoreError::Validation {
                        message: "Native backup object is missing".into(),
                    })
                })
                .map_err(device_store_error)?;
            observed += 1;
        }
        require(
            observed == records,
            "Native backup entry count differs from its manifest",
        )?;
        let rows = builder
            .finish(&expected_fingerprint)
            .map_err(device_store_error)?;
        prepared.push(prepared_section(rows));
    }
    Ok(prepared)
}

fn read_native_object(
    archive: &VerifiedArchive,
    reference: &ObjectReference,
    probe: &dyn CancellationProbe,
) -> Result<Vec<u8>> {
    let capacity = usize::try_from(reference.byte_length).map_err(|_| {
        error(
            "device-metadata-invalid",
            "Native backup object exceeds the addressable range",
        )
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    let copied = archive
        .copy_object(&hex::encode(reference.content_sha256), &mut bytes, probe)
        .map_err(archive_error)?;
    require(
        copied == reference.byte_length && bytes.len() == capacity,
        "Native backup object length differs from its section entry",
    )?;
    Ok(bytes)
}

pub(crate) fn apply_prepared_native_sections(
    store: &mut PersistentStore,
    sections: &[PreparedDeviceSection],
) -> Result<()> {
    for section in sections {
        store
            .device_store_mut()
            .map_err(device_store_error)?
            .restore_prepared_backup_section(section.rows())
            .map_err(device_store_error)?;
    }
    Ok(())
}

pub(crate) fn journal_prepared_native_sections(
    state: &DeviceBackupState,
    id: &str,
    spool: Spool,
    sections: &[PreparedDeviceSection],
) -> Result<Vec<SectionManifest>> {
    let mut stored_objects = BTreeSet::new();
    let mut manifests = Vec::with_capacity(sections.len());
    for section in sections {
        let rows = section.rows();
        let kind = rows.kind();
        let mut fingerprint = FingerprintBuilder::new(&kind.fingerprint_domain());
        rows.visit_entries(|entry, _| {
            let encoded = entry.encode().map_err(|_| StoreError::Validation {
                message: "Native recovery entry is invalid".into(),
            })?;
            fingerprint
                .push(&entry.key, &content_hash(&encoded))
                .map_err(|_| StoreError::Validation {
                    message: "Native recovery fingerprint is invalid".into(),
                })
        })
        .map_err(device_store_error)?;
        let header = NativeSectionHeader {
            profile: NATIVE_SECTION_PROFILE.into(),
            codec: SECTION_CODEC.into(),
            kind,
            content_fingerprint: hex::encode(fingerprint.finish()),
            present: true,
        };
        let header = serde_json::to_string(&header).map_err(|_| {
            error(
                "device-metadata-invalid",
                "Native recovery section header could not be encoded",
            )
        })?;
        state.section_begin(id, spool, kind.id(), &header)?;
        let mut ordinal = 0u64;
        let mut callback_failure = None;
        let visited = rows.visit_entries(|entry, object| {
            let result = (|| -> Result<()> {
                if let (Some(reference), Some(bytes)) = (object_reference(entry), object) {
                    let digest = hex::encode(reference.content_sha256);
                    if stored_objects.insert(digest.clone()) {
                        state.blob_begin(id, spool, &digest)?;
                        let mut offset = 0u64;
                        for chunk in bytes.chunks(MAX_CHUNK_BYTES) {
                            state.blob_append(id, spool, &digest, offset, chunk)?;
                            offset += chunk.len() as u64;
                        }
                        let manifest = state.blob_finish(id, spool, &digest)?;
                        require(
                            manifest.bytes == reference.byte_length && manifest.sha256 == digest,
                            "Native recovery object differs from its section entry",
                        )?;
                    }
                } else {
                    require(
                        object_reference(entry).is_none() && object.is_none(),
                        "Native recovery object presence differs from its section entry",
                    )?;
                }
                let payload = entry.encode().map_err(section_codec_error)?;
                if payload.len() <= MAX_CHUNK_BYTES {
                    let payload = String::from_utf8(payload).map_err(|_| {
                        error(
                            "device-metadata-invalid",
                            "Native recovery entry is not UTF-8",
                        )
                    })?;
                    state.row_append(id, spool, kind.id(), ordinal, &payload)?;
                } else {
                    let digest = hex::encode(content_hash(&payload));
                    state.blob_begin(id, spool, &digest)?;
                    let mut offset = 0u64;
                    for chunk in payload.chunks(MAX_CHUNK_BYTES) {
                        state.blob_append(id, spool, &digest, offset, chunk)?;
                        offset += chunk.len() as u64;
                    }
                    let manifest = state.blob_finish(id, spool, &digest)?;
                    require(
                        manifest.bytes == payload.len() as u64 && manifest.sha256 == digest,
                        "Native recovery entry blob changed while journaling",
                    )?;
                    state.row_append_from_blob(id, spool, kind.id(), ordinal, &digest)?;
                }
                ordinal = ordinal.checked_add(1).ok_or_else(|| {
                    error(
                        "device-metadata-invalid",
                        "Native recovery entry count is exhausted",
                    )
                })?;
                Ok(())
            })();
            match result {
                Ok(()) => Ok(()),
                Err(failure) => {
                    callback_failure = Some(failure);
                    Err(StoreError::Validation {
                        message: "Native recovery journal write failed".into(),
                    })
                }
            }
        });
        if let Some(failure) = callback_failure {
            return Err(failure);
        }
        visited.map_err(device_store_error)?;
        require(
            ordinal == rows.len(),
            "Native recovery journal omitted an entry",
        )?;
        manifests.push(state.section_finish(id, spool, kind.id())?);
    }
    Ok(manifests)
}

pub(crate) fn prepare_journaled_native_sections(
    state: &DeviceBackupState,
    id: &str,
    spool: Spool,
    section_ids: &[String],
) -> Result<Vec<PreparedDeviceSection>> {
    let kinds = selected_native_kinds(section_ids)?;
    let manifests = state.section_list(id, spool)?;
    let manifest_by_id = manifests
        .into_iter()
        .map(|manifest| (manifest.section_id.clone(), manifest))
        .collect::<BTreeMap<_, _>>();
    let mut prepared = Vec::with_capacity(kinds.len());
    for kind in kinds {
        let manifest = manifest_by_id.get(kind.id()).ok_or_else(|| {
            error(
                "device-section-missing",
                "Native recovery section is absent",
            )
        })?;
        require(
            manifest.sealed && manifest.present,
            "Native recovery section is incomplete",
        )?;
        let header = parse_native_header(manifest.metadata_json.as_bytes(), kind)?;
        let expected_fingerprint: [u8; 32] = hex::decode(header.content_fingerprint)
            .map_err(|_| {
                error(
                    "device-metadata-invalid",
                    "Native recovery fingerprint is invalid",
                )
            })?
            .try_into()
            .map_err(|_| {
                error(
                    "device-metadata-invalid",
                    "Native recovery fingerprint has the wrong length",
                )
            })?;
        let mut builder = SectionSpoolBuilder::new_backup(kind).map_err(device_store_error)?;
        let mut after = None;
        let mut observed = 0u64;
        loop {
            let page = state.row_read(id, spool, kind.id(), after, 128)?;
            for row in page.rows {
                require(
                    row.ordinal == observed,
                    "Native recovery entries are not contiguous",
                )?;
                let payload = match row.payload_json {
                    Some(payload) => payload.into_bytes(),
                    None => {
                        let capacity = usize::try_from(row.bytes).map_err(|_| {
                            error(
                                "device-metadata-invalid",
                                "Native recovery entry exceeds the addressable range",
                            )
                        })?;
                        let mut payload = Vec::with_capacity(capacity);
                        while payload.len() < capacity {
                            let bytes = state.row_read_bytes(
                                id,
                                spool,
                                kind.id(),
                                row.ordinal,
                                payload.len() as u64,
                                MAX_CHUNK_BYTES,
                            )?;
                            require(!bytes.is_empty(), "Native recovery entry ended early")?;
                            payload.extend_from_slice(&bytes);
                        }
                        payload
                    }
                };
                let entry = SectionEntry::decode(&payload).map_err(section_codec_error)?;
                let mut object = match object_reference(&entry) {
                    Some(reference) => {
                        let capacity = usize::try_from(reference.byte_length).map_err(|_| {
                            error(
                                "device-metadata-invalid",
                                "Native recovery object exceeds the addressable range",
                            )
                        })?;
                        let digest = hex::encode(reference.content_sha256);
                        let mut bytes = Vec::with_capacity(capacity);
                        while bytes.len() < capacity {
                            let chunk = state.blob_read(
                                id,
                                spool,
                                &digest,
                                bytes.len() as u64,
                                MAX_CHUNK_BYTES,
                            )?;
                            require(!chunk.is_empty(), "Native recovery object ended early")?;
                            bytes.extend_from_slice(&chunk);
                        }
                        Some(bytes)
                    }
                    None => None,
                };
                builder
                    .push_backup_entry(entry, |_| {
                        object.take().ok_or_else(|| StoreError::Validation {
                            message: "Native recovery object is missing".into(),
                        })
                    })
                    .map_err(device_store_error)?;
                after = Some(row.ordinal);
                observed += 1;
            }
            if !page.has_more {
                break;
            }
        }
        require(
            observed == manifest.records,
            "Native recovery entry count differs from its manifest",
        )?;
        prepared.push(prepared_section(
            builder
                .finish(&expected_fingerprint)
                .map_err(device_store_error)?,
        ));
    }
    Ok(prepared)
}

pub(crate) fn resume_journaled_native_restore(
    state: &DeviceBackupState,
    id: &str,
    store: &mut PersistentStore,
) -> Result<i64> {
    let session = state.session(id)?;
    require(
        session.profile == "native-portable"
            && session
                .selected_sections
                .iter()
                .all(|section| SectionKind::parse(section).is_ok()),
        "Native recovery requires a native restore session",
    )?;
    if matches!(session.phase.as_str(), "prepared" | "applying-device") {
        let pending = state.pending_source_sections(id)?;
        if !pending.is_empty() {
            let prepared =
                prepare_journaled_native_sections(state, id, Spool::Source, &pending)?;
            let manifests = state
                .section_list(id, Spool::Source)?
                .into_iter()
                .map(|manifest| (manifest.section_id.clone(), manifest))
                .collect::<BTreeMap<_, _>>();
            for section_id in pending {
                let section = prepared
                    .iter()
                    .find(|section| section.rows().kind().id() == section_id)
                    .ok_or_else(|| {
                        error(
                            "device-section-missing",
                            "Native recovery source section is absent",
                        )
                    })?;
                let manifest = manifests.get(&section_id).ok_or_else(|| {
                    error(
                        "device-section-missing",
                        "Native recovery source manifest is absent",
                    )
                })?;
                state.section_intent(id, &section_id)?;
                store
                    .device_store_mut()
                    .map_err(device_store_error)?
                    .restore_prepared_backup_section(section.rows())
                    .map_err(device_store_error)?;
                state.section_complete(id, &section_id, &manifest.sha256)?;
            }
        }
        if state.session(id)?.phase == "applying-device" {
            state.finish_device(id)?;
        }
    }
    let session = state.session(id)?;
    if session.phase == "committing-library" {
        if state.library_commit_marker_exists(id)? {
            state.mark_library_committed(id)?;
        } else {
            let stage = session.stage_id.as_deref().ok_or_else(|| {
                error(
                    "device-invalid-state",
                    "Native recovery library stage is absent",
                )
            })?;
            let revision = session.expected_revision.ok_or_else(|| {
                error(
                    "device-invalid-state",
                    "Native recovery revision is absent",
                )
            })?;
            let prepared = store
                .prepare_replace_commit(stage, Some(revision))
                .map_err(device_store_error)?;
            let (key, marker) = state.commit_marker(id)?;
            store
                .finish_prepared_replace_with_app_kv(
                    prepared,
                    &key,
                    &serde_json::to_value(marker).map_err(|_| {
                        error(
                            "device-metadata-invalid",
                            "Native recovery marker could not be encoded",
                        )
                    })?,
                )
                .map_err(device_store_error)?;
            state.mark_library_committed(id)?;
        }
    }
    require(
        state.session(id)?.phase == "committed",
        "Native recovery did not reach its committed state",
    )?;
    store.revision().map_err(device_store_error)
}
