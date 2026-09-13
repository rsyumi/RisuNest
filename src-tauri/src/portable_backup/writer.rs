use super::*;
use rusqlite::params;
use std::{fs::File, path::Path};
use zip::{write::FileOptions, CompressionMethod, ZipWriter};

impl Catalog {
    /// Writes a candidate only. Callers must verify it with VerifiedArchive before publication.
    pub(crate) fn write_candidate(
        self,
        path: &Path,
        repair_required: bool,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create_new(true)
            .open(path)?;
        self.write_archive(&mut output, repair_required, probe)?;
        output.sync_all()?;
        Ok(())
    }

    /// Seekable archive construction is separate from publication and durable file sync.
    pub(super) fn write_archive<W: Write + std::io::Seek>(
        self,
        output: &mut W,
        repair_required: bool,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        let mut zip = ZipWriter::new(output);
        let stored = FileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(true);
        zip.start_file("format.json", stored)?;
        serde_json::to_writer(&mut zip, &self.format)?;
        if self.db.is_autocommit() {
            self.db.execute_batch("BEGIN IMMEDIATE")?;
        }
        let mut statement = self.db.prepare("SELECT o.sha256,o.byte_length,s.path FROM objects o JOIN sources s ON o.sha256=s.sha256 ORDER BY o.sha256")?;
        let mut rows = statement.query([])?;
        let mut pack_id = -1_i64;
        let mut offset = 0_u64;
        let mut pack_hash = Sha256::new();
        while let Some(row) = rows.next()? {
            check(probe)?;
            let binary_hash: Vec<u8> = row.get(0)?;
            let hash = hex::encode(&binary_hash);
            let size = sql_u64(row.get(1)?)?;
            let source: String = row.get(2)?;
            if size == 0 {
                if hash != EMPTY_HASH {
                    return Err(Error::Invalid("invalid empty object hash"));
                }
                let mut input = crate::trust_boundary::open_regular_source(Path::new(&source))?;
                if input.read(&mut [0])? != 0 {
                    return Err(Error::Invalid("empty object source grew"));
                }
                continue;
            }
            if pack_id < 0
                || offset
                    .checked_add(size)
                    .ok_or(Error::Invalid("pack range overflow"))?
                    > PACK_BYTES
            {
                if pack_id >= 0 {
                    finish_pack(&self.db, pack_id, offset, pack_hash)?;
                }
                pack_id += 1;
                offset = 0;
                pack_hash = Sha256::new();
                zip.start_file(format!("payloads/{pack_id:06}.bin"), stored)?;
            }
            let mut input = File::open(source)?;
            let mut sink = HashWriter {
                output: &mut zip,
                hash: &mut pack_hash,
            };
            if copy_hash(&mut input, &mut sink, size, probe)? != hash {
                return Err(Error::Invalid("object spool changed"));
            }
            if input.read(&mut [0])? != 0 {
                return Err(Error::Invalid("object source grew"));
            }
            self.db.execute(
                "UPDATE objects SET pack_id=?1,offset=?2 WHERE sha256=?3",
                params![pack_id, offset as i64, binary_hash],
            )?;
            offset = offset
                .checked_add(size)
                .ok_or(Error::Invalid("pack range overflow"))?;
        }
        if pack_id >= 0 {
            finish_pack(&self.db, pack_id, offset, pack_hash)?;
        }
        drop(rows);
        drop(statement);
        let count = |table: &str| -> Result<String> {
            let value: i64 =
                self.db
                    .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
            Ok(sql_u64(value)?.to_string())
        };
        let mut manifest = Manifest {
            capture_id: self.format.capture_id.clone(),
            catalog_bytes: String::new(),
            catalog_sha256: String::new(),
            pack_count: count("packs")?,
            object_count: count("objects")?,
            file_count: count("files")?,
            repair_required,
            library_included: self.db.query_row(
                "SELECT value='true' FROM backup_info WHERE key='libraryIncluded'",
                [],
                |r| r.get(0),
            )?,
            device_included: self.db.query_row(
                "SELECT EXISTS(SELECT 1 FROM device_sections WHERE included=1)",
                [],
                |r| r.get(0),
            )?,
            profile: match self
                .db
                .query_row::<String, _, _>(
                    "SELECT value FROM backup_info WHERE key='profile'",
                    [],
                    |r| r.get(0),
                )?
                .as_str()
            {
                "portable" => Profile::Portable,
                "source-sqlite" => Profile::SourceSqlite,
                _ => return Err(Error::Invalid("invalid archive profile")),
            },
        };
        // An incomplete inventory cannot be reported as a normal backup.
        let diagnostics: i64 = self
            .db
            .query_row("SELECT count(*) FROM diagnostics", [], |r| r.get(0))?;
        manifest.repair_required |= diagnostics != 0;
        self.db.execute_batch("DROP TABLE temp.sources")?;
        self.db.execute_batch("COMMIT")?;
        self.db.close().map_err(|(_, error)| Error::Sql(error))?;
        let catalog_path = self.directory.path().join("archive.sqlite");
        let mut catalog = File::open(&catalog_path)?;
        let length = catalog.metadata()?.len();
        zip.start_file(
            "archive.sqlite",
            FileOptions::default()
                .compression_method(CompressionMethod::Deflated)
                .compression_level(Some(1))
                .large_file(true),
        )?;
        manifest.catalog_sha256 = copy_hash(&mut catalog, &mut zip, length, probe)?;
        manifest.catalog_bytes = length.to_string();
        zip.start_file("manifest.json", stored)?;
        serde_json::to_writer(&mut zip, &manifest)?;
        check(probe)?;
        zip.finish()?;
        Ok(())
    }
}

fn finish_pack(db: &rusqlite::Connection, id: i64, length: u64, hash: Sha256) -> Result<()> {
    db.execute(
        "INSERT INTO packs VALUES(?1,?2,?3,?4)",
        params![
            id,
            format!("payloads/{id:06}.bin"),
            i64::try_from(length).map_err(|_| Error::Invalid("pack exceeds SQLite range"))?,
            hex::encode(hash.finalize())
        ],
    )?;
    Ok(())
}
struct HashWriter<'a, W> {
    output: &'a mut W,
    hash: &'a mut Sha256,
}
impl<W: Write> Write for HashWriter<'_, W> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let n = self.output.write(buffer)?;
        self.hash.update(&buffer[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}
