//! Synthetic format comparison only. Never linked into the application.
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::{
    env,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    time::Instant,
};
use zip::{write::FileOptions, CompressionMethod, ZipArchive, ZipWriter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("pack");
    let workload = args.get(2).map(String::as_str).unwrap_or("small");
    assert!(matches!(mode, "pack" | "entries"));
    let (count, bytes) = match workload {
        "small" => (1_000_u64, 1_024_usize),
        "many" => (100_000, 1_024),
        "large" => (8, 32 * 1024 * 1024),
        _ => panic!("unknown synthetic workload"),
    };
    let dir = tempfile::tempdir()?;
    let database = dir.path().join("archive.sqlite");
    let archive_path = dir.path().join("archive.zip");
    let db = Connection::open(&database)?;
    db.execute_batch("PRAGMA journal_mode=DELETE; PRAGMA cache_size=-16384;
        CREATE TABLE objects(id INTEGER PRIMARY KEY, entry TEXT, offset INTEGER, size INTEGER, hash BLOB);
        CREATE TABLE files(key TEXT PRIMARY KEY, object_id INTEGER, metadata TEXT);")?;
    let start = Instant::now();
    let mut zip = ZipWriter::new(File::create(&archive_path)?);
    let options = FileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .large_file(true);
    let mut pack = 0_u64;
    let mut offset = 0_u64;
    let mut buffer = vec![0_u8; bytes];
    let mut insert = db.prepare("INSERT INTO objects VALUES(?1,?2,?3,?4,?5)")?;
    let mut alias = db.prepare("INSERT INTO files VALUES(?1,?2,?3)")?;
    db.execute_batch("BEGIN")?;
    for id in 0..count {
        // Includes one empty file and two aliases with distinct metadata per object.
        let size = if id == 0 { 0 } else { bytes };
        let payload = &mut buffer[..size];
        for (index, chunk) in payload.chunks_mut(32).enumerate() {
            let mut hash = Sha256::new();
            hash.update(id.to_le_bytes());
            hash.update((index as u64).to_le_bytes());
            let value = hash.finalize();
            chunk.copy_from_slice(&value[..chunk.len()]);
        }
        let hash = Sha256::digest(&*payload);
        if mode == "pack" && offset + size as u64 > 256 * 1024 * 1024 {
            pack += 1;
            offset = 0;
        }
        let entry = if mode == "pack" {
            format!("payloads/{pack:06}.bin")
        } else {
            format!("objects/{id:08}")
        };
        if mode == "entries" || offset == 0 && size != 0 {
            zip.start_file(&entry, options)?;
        }
        if size > 0 {
            zip.write_all(payload)?;
        }
        insert.execute(params![
            id as i64,
            if size == 0 { "" } else { &entry },
            offset as i64,
            size as i64,
            hash.as_slice()
        ])?;
        alias.execute(params![
            format!("asset/{id}"),
            id as i64,
            "{\"kind\":\"asset\"}"
        ])?;
        alias.execute(params![
            format!("inlay/{id}"),
            id as i64,
            "{\"kind\":\"inlay\"}"
        ])?;
        if mode == "pack" {
            offset += size as u64;
        }
    }
    db.execute_batch("COMMIT")?;
    drop(insert);
    drop(alias);
    db.close().map_err(|(_, error)| error)?;
    zip.start_file(
        "archive.sqlite",
        FileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .compression_level(Some(1))
            .large_file(true),
    )?;
    let db_hash = hash_file(&database)?;
    std::io::copy(&mut File::open(&database)?, &mut zip)?;
    zip.finish()?.sync_all()?;
    let write_ms = start.elapsed().as_millis();
    let verify_start = Instant::now();
    verify(&archive_path, dir.path(), &db_hash, count, false)?;
    let verify_ms = verify_start.elapsed().as_millis();
    let restore_start = Instant::now();
    verify(&archive_path, dir.path(), &db_hash, count, true)?;
    let restore_ms = restore_start.elapsed().as_millis();
    let catalog_bytes = database.metadata()?.len();
    let archive_bytes = archive_path.metadata()?.len();
    println!(
        "{}",
        serde_json::json!({"mode":mode,"workload":workload,"objects":count,"aliases":count*2,
        "writeMs":write_ms,"verifyMs":verify_ms,"createAndVerifyMs":write_ms+verify_ms,"verifyAndExtractMs":restore_ms,
        "archiveBytes":archive_bytes,"catalogBytes":catalog_bytes,"maxTrackedTemporaryBytes":catalog_bytes*2+bytes as u64,"peakWorkingSetBytes":peak_working_set(),
        "note":"Synthetic payload generation included in write time; extraction writes each verified object to one reusable scratch file. OS peak working set measured by launcher."})
    );
    Ok(())
}

#[cfg(windows)]
fn peak_working_set() -> Option<usize> {
    #[repr(C)]
    struct Counters {
        size: u32,
        page_faults: u32,
        peak: usize,
        working_set: usize,
        paged_peak: usize,
        paged: usize,
        nonpaged_peak: usize,
        nonpaged: usize,
        pagefile: usize,
        pagefile_peak: usize,
    }
    #[link(name = "psapi")]
    extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut Counters,
            size: u32,
        ) -> i32;
    }
    let mut counters = Counters {
        size: std::mem::size_of::<Counters>() as u32,
        page_faults: 0,
        peak: 0,
        working_set: 0,
        paged_peak: 0,
        paged: 0,
        nonpaged_peak: 0,
        nonpaged: 0,
        pagefile: 0,
        pagefile_peak: 0,
    };
    // The -1 pseudo handle denotes this process; the OS writes only the sized stack structure.
    let ok = unsafe {
        GetProcessMemoryInfo(
            -1_isize as *mut _,
            &mut counters,
            std::mem::size_of::<Counters>() as u32,
        )
    };
    (ok != 0).then_some(counters.peak)
}
#[cfg(not(windows))]
fn peak_working_set() -> Option<usize> {
    None
}

fn hash_file(path: &std::path::Path) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    Ok(hash.finalize().to_vec())
}

fn verify(
    path: &std::path::Path,
    directory: &std::path::Path,
    expected_db_hash: &[u8],
    count: u64,
    extract: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut zip = ZipArchive::new(File::open(path)?)?;
    let catalog = directory.join("verified.sqlite");
    {
        let mut input = zip.by_name("archive.sqlite")?;
        std::io::copy(&mut input, &mut File::create(&catalog)?)?;
    }
    assert_eq!(hash_file(&catalog)?, expected_db_hash);
    let db = Connection::open(&catalog)?;
    let aliases: i64 = db.query_row("SELECT count(*) FROM files", [], |row| row.get(0))?;
    assert_eq!(u64::try_from(aliases)?, count * 2);
    let invalid: i64 = db.query_row("SELECT count(*) FROM files f LEFT JOIN objects o ON f.object_id=o.id WHERE o.id IS NULL OR f.metadata != CASE WHEN f.key LIKE 'asset/%' THEN '{\"kind\":\"asset\"}' ELSE '{\"kind\":\"inlay\"}' END", [], |row| row.get(0))?;
    assert_eq!(invalid, 0);
    let mut statement = db.prepare("SELECT entry, offset, size, hash FROM objects ORDER BY id")?;
    let mut rows = statement.query([])?;
    let mut file = File::open(path)?;
    let mut buffer = vec![0; 1024 * 1024];
    while let Some(row) = rows.next()? {
        let entry: String = row.get(0)?;
        let offset = u64::try_from(row.get::<_, i64>(1)?)?;
        let size = u64::try_from(row.get::<_, i64>(2)?)?;
        let expected: Vec<u8> = row.get(3)?;
        if size > 0 {
            let entry = zip.by_name(&entry)?;
            assert_eq!(entry.compression(), CompressionMethod::Stored);
            assert!(offset.checked_add(size).unwrap() <= entry.size());
            file.seek(SeekFrom::Start(
                entry.data_start().checked_add(offset).unwrap(),
            ))?;
        }
        let mut output = if extract {
            Some(File::create(directory.join("extracted-object"))?)
        } else {
            None
        };
        let mut remaining = size;
        let mut hash = Sha256::new();
        while remaining > 0 {
            let requested = remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..requested])?;
            hash.update(&buffer[..requested]);
            if let Some(output) = output.as_mut() {
                output.write_all(&buffer[..requested])?;
            }
            remaining -= requested as u64;
        }
        assert_eq!(hash.finalize().as_slice(), expected);
    }
    Ok(())
}
