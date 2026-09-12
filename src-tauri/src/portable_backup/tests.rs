use super::*;
use crate::persistent_store::portable::digest_raw_tables;
use std::{
    fs::{self, File},
    io::{Seek, SeekFrom, Write},
};
struct Never;
impl CancellationProbe for Never {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[test]
fn preservation_keeps_distinct_source_paths_even_when_bytes_are_live() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = fixture(directory.path());
    let hash = hex::encode(Sha256::digest(b"synthetic file bytes"));
    for (key, metadata) in [
        (
            "assets/orphan.bin".to_owned(),
            "{\"storage\":\"unclassified\"}",
        ),
        (
            format!("assets-v2/objects/{}/{}", &hash[..2], &hash[2..]),
            "{\"storage\":\"cas\"}",
        ),
    ] {
        catalog
            .add_file(
                "preserved",
                &key,
                metadata,
                Some((&directory.path().join("payload"), 20)),
                None,
                &Never,
            )
            .unwrap();
    }
    let path = directory.path().join("backup.risunest");
    catalog.write_candidate(&path, false, &Never).unwrap();
    let archive =
        VerifiedArchive::open(File::open(path).unwrap(), directory.path(), &Never).unwrap();
    let inventory = RestoreInventory::build(&archive, directory.path(), &Never).unwrap();
    let report = inventory
        .preserve(&archive, directory.path(), &Never)
        .unwrap()
        .unwrap();
    assert_eq!(report.files, "1");
    assert_eq!(report.bytes, "20");
    let index = rusqlite::Connection::open(std::path::Path::new(&report.path).join("index.sqlite"))
        .unwrap();
    let (key, metadata): (String, String) = index
        .query_row("SELECT logical_key,metadata FROM source_files", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert!(key.ends_with("/assets/orphan.bin"));
    assert_eq!(metadata, "{\"storage\":\"unclassified\"}");
    assert_eq!(
        fs::read(
            std::path::Path::new(&report.path)
                .join("objects")
                .join(hash)
        )
        .unwrap(),
        b"synthetic file bytes"
    );
}

fn fixture(directory: &std::path::Path) -> Catalog {
    let catalog = Catalog::create(directory, "synthetic-test", 7).unwrap();
    catalog
        .db
        .execute("INSERT INTO root VALUES(?1)", ["{ \"z\":null, \"a\":[] }"])
        .unwrap();
    fs::write(directory.join("payload"), b"synthetic file bytes").unwrap();
    fs::write(directory.join("empty"), b"").unwrap();
    for (kind, key, metadata) in [
        ("asset", "assets/a", "{\"x\":1}"),
        ("inlay", "shared", "{\"x\":2}"),
    ] {
        catalog
            .add_file(
                kind,
                key,
                metadata,
                Some((&directory.join("payload"), 20)),
                None,
                &Never,
            )
            .unwrap();
    }
    catalog
        .add_file(
            "asset",
            "empty",
            "{}",
            Some((&directory.join("empty"), 0)),
            None,
            &Never,
        )
        .unwrap();
    catalog
}

#[test]
fn zip_sqlite_round_trip_preserves_raw_values_aliases_and_empty_objects() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("backup.risunest");
    let catalog = fixture(directory.path());
    let before = digest_raw_tables(&catalog.db, &Never).unwrap();
    catalog.write_candidate(&path, false, &Never).unwrap();
    let archive =
        VerifiedArchive::open(File::open(&path).unwrap(), directory.path(), &Never).unwrap();
    assert_eq!(archive.manifest.object_count, "2");
    assert_eq!(archive.manifest.file_count, "3");
    assert_eq!(archive.manifest.pack_count, "1");
    assert!(!archive.manifest.device_included);
    assert_eq!(before, digest_raw_tables(&archive.db, &Never).unwrap());
    let hashes = archive
        .db
        .prepare("SELECT sha256 FROM objects ORDER BY sha256")
        .unwrap()
        .query_map([], |r| r.get::<_, Vec<u8>>(0).map(hex::encode))
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    for hash in hashes {
        let mut bytes = Vec::new();
        let size = archive.copy_object(&hash, &mut bytes, &Never).unwrap();
        assert_eq!(size, bytes.len() as u64);
        assert_eq!(hex::encode(Sha256::digest(&bytes)), hash);
    }
    let metadata=archive.db.prepare("SELECT metadata FROM files WHERE object_hash IS NOT NULL AND logical_key!='empty' ORDER BY kind").unwrap().query_map([],|r|r.get::<_,String>(0)).unwrap().collect::<std::result::Result<Vec<_>,_>>().unwrap();
    assert_eq!(metadata, vec!["{\"x\":1}", "{\"x\":2}"]);
}

#[test]
fn corrupt_payload_and_truncated_directory_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("backup.risunest");
    fixture(directory.path())
        .write_candidate(&path, false, &Never)
        .unwrap();
    let mut zip = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
    let start = zip.by_name("payloads/000000.bin").unwrap().data_start();
    drop(zip);
    {
        let mut file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(start)).unwrap();
        file.write_all(b"X").unwrap();
    }
    assert!(VerifiedArchive::open(File::open(&path).unwrap(), directory.path(), &Never).is_err());
    {
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(file.metadata().unwrap().len() - 4).unwrap();
    }
    assert!(VerifiedArchive::open(File::open(&path).unwrap(), directory.path(), &Never).is_err());
}

#[test]
fn changed_empty_source_is_rejected_before_publication() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = fixture(directory.path());
    let source: String = catalog
        .db
        .query_row(
            "SELECT path FROM sources JOIN objects USING(sha256) WHERE byte_length=0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    fs::write(source, b"changed after capture").unwrap();
    assert!(catalog
        .write_candidate(&directory.path().join("candidate.part"), false, &Never)
        .is_err());
}

#[test]
fn missing_and_empty_are_distinct_and_force_repair_status() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("backup.risunest");
    let catalog = fixture(directory.path());
    catalog
        .add_file("asset", "absent", "{}", None, None, &Never)
        .unwrap();
    catalog.write_candidate(&path, false, &Never).unwrap();
    let archive =
        VerifiedArchive::open(File::open(&path).unwrap(), directory.path(), &Never).unwrap();
    assert!(archive.manifest.repair_required);
    assert_eq!(
        archive
            .db
            .query_row(
                "SELECT state FROM files WHERE logical_key='absent'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "missing"
    );
    assert_eq!(
        archive
            .db
            .query_row(
                "SELECT state FROM files WHERE logical_key='empty'",
                [],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "present"
    );
}

#[test]
fn source_hash_and_length_mismatch_never_become_successful_objects() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = Catalog::create(directory.path(), "synthetic", 0).unwrap();
    let source = directory.path().join("source");
    fs::write(&source, b"abc").unwrap();
    for (size, hash) in [(2, None), (4, None), (3, Some(EMPTY_HASH))] {
        assert!(catalog
            .add_file("asset", "a", "{}", Some((&source, size)), hash, &Never)
            .is_err());
    }
    assert_eq!(
        catalog
            .db
            .query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn external_sql_objects_are_rejected_without_executing_them() {
    let directory = tempfile::tempdir().unwrap();
    let catalog = Catalog::create(directory.path(), "synthetic", 0).unwrap();
    catalog
        .db
        .execute_batch("CREATE VIEW injected AS SELECT 1")
        .unwrap();
    assert!(catalog::verify_schema(&catalog.db).is_err());
}

#[test]
fn decimals_do_not_round_or_accept_noncanonical_values() {
    assert_eq!(decimal("9007199254740993").unwrap(), 9_007_199_254_740_993);
    for invalid in ["-1", "01", "1.0", "1e4", "", "9223372036854775808"] {
        assert!(decimal(invalid).is_err());
    }
}

/// Rebuild a synthetic archive with a catalog mutation and an updated catalog hash. These tests
/// exercise semantic/range verification, not just the outer SHA mismatch check.
fn mutate_catalog(source: &std::path::Path, output: &std::path::Path, sql: &str) {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("mutated.sqlite");
    let mut archive = zip::ZipArchive::new(File::open(source).unwrap()).unwrap();
    let mut parts = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let name = entry.name().to_owned();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        parts.push((name, bytes));
    }
    let index = parts
        .iter()
        .position(|(name, _)| name == "archive.sqlite")
        .unwrap();
    fs::write(&db_path, &parts[index].1).unwrap();
    let db = rusqlite::Connection::open(&db_path).unwrap();
    db.execute_batch(sql).unwrap();
    db.close().unwrap();
    parts[index].1 = fs::read(&db_path).unwrap();
    let hash = hex::encode(Sha256::digest(&parts[index].1));
    let size = parts[index].1.len().to_string();
    let manifest_index = parts
        .iter()
        .position(|(name, _)| name == "manifest.json")
        .unwrap();
    let mut manifest: Manifest = serde_json::from_slice(&parts[manifest_index].1).unwrap();
    manifest.catalog_sha256 = hash;
    manifest.catalog_bytes = size;
    parts[manifest_index].1 = serde_json::to_vec(&manifest).unwrap();
    let mut zip = zip::ZipWriter::new(File::create(output).unwrap());
    for (name, bytes) in parts {
        zip.start_file(
            name,
            zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .large_file(true),
        )
        .unwrap();
        zip.write_all(&bytes).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn correctly_hashed_catalog_still_rejects_invalid_ranges_references_and_sql() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.risunest");
    fixture(directory.path())
        .write_candidate(&source, false, &Never)
        .unwrap();
    for (index, sql) in [
        "UPDATE objects SET offset=-1 WHERE byte_length>0",
        "UPDATE objects SET offset=9223372036854775807 WHERE byte_length>0",
        "UPDATE objects SET byte_length=byte_length+1 WHERE byte_length>0",
        "UPDATE objects SET pack_id=17 WHERE byte_length>0",
        "UPDATE objects SET offset=1 WHERE byte_length=0",
        "UPDATE files SET object_hash=zeroblob(32) WHERE logical_key='empty'",
        "UPDATE files SET expected_hash='invalid' WHERE logical_key='empty'",
        "CREATE TRIGGER injected AFTER INSERT ON root BEGIN DELETE FROM root; END",
        "ALTER TABLE root ADD COLUMN unexpected TEXT",
    ]
    .iter()
    .enumerate()
    {
        let path = directory.path().join(format!("mutated-{index}.risunest"));
        mutate_catalog(&source, &path, sql);
        assert!(
            VerifiedArchive::open(File::open(path).unwrap(), directory.path(), &Never).is_err(),
            "mutation {index} was accepted"
        );
    }
}

#[test]
fn duplicate_and_unexpected_zip_names_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.risunest");
    fixture(directory.path())
        .write_candidate(&source, false, &Never)
        .unwrap();
    for (index, name) in ["format.json", "../outside", "unknown.txt"]
        .iter()
        .enumerate()
    {
        let destination = directory.path().join(format!("extra-{index}.risunest"));
        fs::copy(&source, &destination).unwrap();
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&destination)
            .unwrap();
        let mut writer = zip::ZipWriter::new_append(file).unwrap();
        writer
            .start_file(*name, zip::write::FileOptions::default())
            .unwrap();
        writer.write_all(b"{}").unwrap();
        writer.finish().unwrap();
        assert!(
            VerifiedArchive::open(File::open(destination).unwrap(), directory.path(), &Never)
                .is_err()
        );
    }
}

#[test]
fn candidates_never_overwrite_an_existing_destination() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("existing");
    fs::write(&path, b"existing destination").unwrap();
    assert!(fixture(directory.path())
        .write_candidate(&path, false, &Never)
        .is_err());
    assert_eq!(fs::read(path).unwrap(), b"existing destination");
}

#[test]
fn disk_full_during_each_archive_region_cannot_produce_a_valid_backup() {
    struct DiskFull {
        file: File,
        remaining: u64,
    }
    impl Write for DiskFull {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "synthetic disk full",
                ));
            }
            let size = bytes.len().min(self.remaining as usize);
            let written = self.file.write(&bytes[..size])?;
            self.remaining -= written as u64;
            Ok(written)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.file.flush()
        }
    }
    impl Seek for DiskFull {
        fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
            self.file.seek(from)
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let reference = directory.path().join("reference.risunest");
    fixture(directory.path())
        .write_candidate(&reference, false, &Never)
        .unwrap();
    let size = fs::metadata(reference).unwrap().len();
    for budget in [32, size / 2, size - 64] {
        let path = directory.path().join(format!("failed-{budget}.part"));
        let mut output = DiskFull {
            file: File::create(&path).unwrap(),
            remaining: budget,
        };
        assert!(fixture(directory.path())
            .write_archive(&mut output, false, &Never)
            .is_err());
        drop(output);
        assert!(
            VerifiedArchive::open(File::open(path).unwrap(), directory.path(), &Never).is_err()
        );
    }
}

#[test]
fn cancellation_during_archive_construction_cannot_produce_a_valid_backup() {
    struct Cancel(std::cell::Cell<usize>);
    impl CancellationProbe for Cancel {
        fn is_cancelled(&self) -> bool {
            let calls = self.0.get() + 1;
            self.0.set(calls);
            calls > 3
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cancelled.part");
    assert!(matches!(
        fixture(directory.path()).write_candidate(&path, false, &Cancel(std::cell::Cell::new(0))),
        Err(Error::Cancelled)
    ));
    assert!(VerifiedArchive::open(File::open(path).unwrap(), directory.path(), &Never).is_err());
}
