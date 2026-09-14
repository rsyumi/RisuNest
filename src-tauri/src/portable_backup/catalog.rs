use super::*;
use crate::persistent_store::portable::{create_raw_tables, TABLES};
use rusqlite::{params, Connection, OptionalExtension};
use std::{fs::File, path::Path};

pub(super) const SCHEMA: &[(&str, &str)] = &[
    ("backup_info", "CREATE TABLE backup_info (key TEXT PRIMARY KEY, value TEXT NOT NULL)"),
    ("packs", "CREATE TABLE packs (id INTEGER PRIMARY KEY, entry TEXT NOT NULL UNIQUE, byte_length INTEGER NOT NULL, sha256 TEXT NOT NULL)"),
    ("objects", "CREATE TABLE objects (sha256 BLOB PRIMARY KEY NOT NULL CHECK(length(sha256)=32), pack_id INTEGER, offset INTEGER NOT NULL, byte_length INTEGER NOT NULL)"),
    ("files", "CREATE TABLE files (kind TEXT NOT NULL, logical_key TEXT NOT NULL, object_hash BLOB, expected_hash TEXT, metadata TEXT NOT NULL, state TEXT NOT NULL, PRIMARY KEY(kind, logical_key))"),
    ("diagnostics", "CREATE TABLE diagnostics (code TEXT NOT NULL, subject TEXT NOT NULL)"),
    ("device_sections", "CREATE TABLE device_sections (section TEXT PRIMARY KEY, schema_version INTEGER NOT NULL, included INTEGER NOT NULL, complete INTEGER NOT NULL, present INTEGER NOT NULL, record_count INTEGER NOT NULL, sha256 TEXT NOT NULL)"),
    ("device_records", "CREATE TABLE device_records (section TEXT NOT NULL, ordinal INTEGER NOT NULL, metadata TEXT NOT NULL, PRIMARY KEY(section, ordinal))"),
];

pub(crate) struct Catalog {
    pub(crate) db: Connection,
    pub(super) directory: tempfile::TempDir,
    pub(super) format: Format,
}

impl Catalog {
    /// Copy a sealed device stream into the job's immutable spool. The supplied length and
    /// digest are checked before its object can become part of the archive.
    pub(crate) fn add_reader(
        &self,
        kind: &str,
        key: &str,
        metadata: &str,
        input: &mut dyn Read,
        size: u64,
        expected_hash: &str,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        if !hash_valid(expected_hash) {
            return Err(Error::Invalid("invalid stream object hash"));
        }
        let mut spool = tempfile::NamedTempFile::new_in(self.directory.path())?;
        if copy_hash(input, &mut spool, size, probe)? != expected_hash {
            return Err(Error::Invalid("stream object hash mismatch"));
        }
        let mut extra = [0];
        if input.read(&mut extra)? != 0 {
            return Err(Error::Invalid("stream object length mismatch"));
        }
        spool.as_file().sync_all()?;
        let path = spool.into_temp_path();
        if self.add_pinned_file(kind, key, metadata, &path, size, expected_hash, probe)? {
            // The catalog owns every stream spool until the archive writer has consumed it.
            path.keep().map_err(|error| Error::Io(error.error))?;
        }
        Ok(())
    }
    /// Group a large inventory into one bounded-page-cache SQLite transaction. Raw generation
    /// capture uses its own transaction and must finish before this phase starts.
    pub(crate) fn begin_inventory(&self) -> Result<()> {
        self.db.execute_batch("BEGIN IMMEDIATE")?;
        Ok(())
    }

    /// Reference immutable CAS bytes without duplicating the whole library in a spool. The job
    /// must keep its revision lease/durable CAS pins until the candidate is finished. Hashes are
    /// verified during writing, including when this descriptor came from a trusted CAS name.
    pub(crate) fn add_pinned_file(
        &self,
        kind: &str,
        key: &str,
        metadata: &str,
        path: &Path,
        size: u64,
        hash: &str,
        probe: &dyn CancellationProbe,
    ) -> Result<bool> {
        check(probe)?;
        if !matches!(
            kind,
            "asset" | "inlay" | "cold" | "owner" | "preserved" | "device"
        ) || key.is_empty()
            || key.contains('\0')
            || !hash_valid(hash)
            || size == 0 && hash != EMPTY_HASH
        {
            return Err(Error::Invalid("invalid pinned file descriptor"));
        }
        if File::open(path)?.metadata()?.len() != size {
            return Err(Error::Invalid("pinned object length changed"));
        }
        let size = i64::try_from(size)
            .map_err(|_| Error::Invalid("object exceeds SQLite length range"))?;
        let bytes = hex::decode(hash).map_err(|_| Error::Invalid("invalid pinned object hash"))?;
        let existing: Option<i64> = self
            .db
            .query_row(
                "SELECT byte_length FROM objects WHERE sha256=?1",
                [&bytes],
                |r| r.get(0),
            )
            .optional()?;
        let source_added = match existing {
            Some(existing) if existing != size => {
                return Err(Error::Invalid("conflicting pinned object lengths"))
            }
            None => {
                self.db.execute(
                    "INSERT INTO objects VALUES(?1,NULL,0,?2)",
                    params![bytes, size],
                )?;
                self.db.execute(
                    "INSERT INTO sources VALUES(?1,?2)",
                    params![
                        bytes,
                        path.to_str()
                            .ok_or(Error::Invalid("non-Unicode pinned path"))?
                    ],
                )?;
                true
            }
            _ => false,
        };
        self.db.execute(
            "INSERT INTO files VALUES(?1,?2,?3,?4,?5,'present')",
            params![kind, key, bytes, hash, metadata],
        )?;
        Ok(source_added)
    }

    pub(crate) fn create(
        job_directory: &Path,
        source_build: &str,
        source_revision: i64,
    ) -> Result<Self> {
        if source_build.len() > 256 || source_revision < 0 {
            return Err(Error::Invalid("invalid capture provenance"));
        }
        let directory = tempfile::Builder::new()
            .prefix("portable-")
            .tempdir_in(job_directory)?;
        let db = Connection::open(directory.path().join("archive.sqlite"))?;
        db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-16384; PRAGMA temp_store=FILE; PRAGMA trusted_schema=OFF;")?;
        create_raw_tables(&db)?;
        for (_, sql) in SCHEMA {
            db.execute_batch(sql)?;
        }
        // This is an operational index in the connection's temporary database, never archived.
        db.execute_batch(
            "CREATE TEMP TABLE sources (sha256 BLOB PRIMARY KEY, path TEXT NOT NULL)",
        )?;
        let format = Format {
            magic: FORMAT.into(),
            format_version: VERSION,
            sqlite_schema_version: VERSION,
            source_app_build: source_build.into(),
            capture_id: uuid::Uuid::new_v4().to_string(),
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| Error::Invalid("clock precedes Unix epoch"))?
                .as_millis()
                .to_string(),
        };
        db.execute(
            "INSERT INTO backup_info VALUES('captureId',?1)",
            [&format.capture_id],
        )?;
        db.execute(
            "INSERT INTO backup_info VALUES('sourceRevision',?1)",
            [source_revision.to_string()],
        )?;
        db.execute_batch(
            "INSERT INTO backup_info VALUES('profile','portable'),('libraryIncluded','true')",
        )?;
        Ok(Self {
            db,
            directory,
            format,
        })
    }

    /// Input must be an immutable job-owned spool or a pinned CAS object. This primitive verifies
    /// its declared bytes and hash; the capture adapter owns mutable-source identity checks.
    pub(crate) fn add_file(
        &self,
        kind: &str,
        key: &str,
        metadata: &str,
        source: Option<(&Path, u64)>,
        expected_hash: Option<&str>,
        probe: &dyn CancellationProbe,
    ) -> Result<()> {
        check(probe)?;
        if !matches!(
            kind,
            "asset" | "inlay" | "cold" | "owner" | "preserved" | "device"
        ) || key.is_empty()
            || key.contains('\0')
            || expected_hash.is_some_and(|v| !hash_valid(v))
        {
            return Err(Error::Invalid("invalid logical file descriptor"));
        }
        let Some((path, size)) = source else {
            self.db.execute(
                "INSERT INTO files VALUES(?1,?2,NULL,?3,?4,'missing')",
                params![kind, key, expected_hash, metadata],
            )?;
            self.db
                .execute("INSERT INTO diagnostics VALUES('missing-file',?1)", [key])?;
            return Ok(());
        };
        let size_i64 = i64::try_from(size)
            .map_err(|_| Error::Invalid("object exceeds SQLite length range"))?;
        let mut input = crate::trust_boundary::open_regular_source(path)?;
        let identity = crate::asset_repository::exact_file_identity(&input)?;
        let mut spool = tempfile::NamedTempFile::new_in(self.directory.path())?;
        let hash = copy_hash(&mut input, &mut spool, size, probe)?;
        let mut extra = [0];
        if input.read(&mut extra)? != 0 || expected_hash.is_some_and(|v| v != hash) {
            return Err(Error::Invalid("source length or hash changed"));
        }
        if crate::asset_repository::exact_file_identity(&input)? != identity
            || crate::asset_repository::exact_file_identity(
                &crate::trust_boundary::open_regular_source(path)?,
            )? != identity
        {
            return Err(Error::Invalid("source identity changed during capture"));
        }
        let binary_hash = hex::decode(&hash).map_err(|_| Error::Invalid("invalid object hash"))?;
        let existing: Option<i64> = self
            .db
            .query_row(
                "SELECT byte_length FROM objects WHERE sha256=?1",
                [&binary_hash],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing != size_i64 {
                return Err(Error::Invalid("object hash collision"));
            }
        } else {
            spool.as_file().sync_all()?;
            let destination = self.directory.path().join(&hash);
            spool
                .persist_noclobber(&destination)
                .map_err(|e| Error::Io(e.error))?;
            self.db.execute(
                "INSERT INTO objects VALUES(?1,NULL,0,?2)",
                params![binary_hash, size_i64],
            )?;
            self.db.execute(
                "INSERT INTO sources VALUES(?1,?2)",
                params![
                    binary_hash,
                    destination
                        .to_str()
                        .ok_or(Error::Invalid("non-Unicode spool path"))?
                ],
            )?;
        }
        self.db.execute(
            "INSERT INTO files VALUES(?1,?2,?3,?4,?5,'present')",
            params![kind, key, binary_hash, expected_hash, metadata],
        )?;
        Ok(())
    }
}

pub(super) fn verify_schema(db: &Connection) -> Result<()> {
    let mut expected = std::collections::BTreeMap::new();
    for table in TABLES {
        expected.insert(table.name.to_string(), ("table", table.create_sql()));
        if let Some((name, sql)) = table.index_sql() {
            expected.insert(name, ("index", sql));
        }
    }
    for (name, sql) in SCHEMA {
        expected.insert(name.to_string(), ("table", sql.to_string()));
    }
    let mut statement =
        db.prepare("SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let kind: String = row.get(0)?;
        let name: String = row.get(1)?;
        let sql: String = row.get(2)?;
        if expected
            .remove(&name)
            .is_none_or(|(expected_kind, expected_sql)| {
                kind != expected_kind || sql != expected_sql
            })
        {
            return Err(Error::Invalid("unexpected archive SQLite schema"));
        }
    }
    if !expected.is_empty() {
        return Err(Error::Invalid("incomplete archive SQLite schema"));
    }
    let integrity: String = db.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if integrity != "ok" {
        return Err(Error::Invalid("archive SQLite integrity failure"));
    }
    Ok(())
}
