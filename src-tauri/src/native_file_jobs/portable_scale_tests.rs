//! Explicitly invoked synthetic acceptance harness, never part of product runtime.
use super::*;
use crate::local_backup::NeverCancelled;
use crate::persistent_store::portable::digest_raw_tables;
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::time::Instant;

#[test]
#[ignore = "large synthetic disk/RSS acceptance; set RISUNEST_BACKUP_SCALE=database-1g|files-100k|payload-4g"]
fn native_portable_scale_acceptance() {
    let mode = std::env::var("RISUNEST_BACKUP_SCALE").expect("explicit synthetic scale scenario");
    assert!(matches!(
        mode.as_str(),
        "database-1g" | "database-probe" | "files-100k" | "payload-4g" | "portable-small"
    ));
    let work = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join(".tmp");
    fs::create_dir_all(&work).unwrap();
    let directory = tempfile::Builder::new()
        .prefix("portable-scale-")
        .tempdir_in(&work)
        .unwrap();
    let source = directory.path().join("source");
    let target = directory.path().join("target");
    let jobs = directory.path().join("export-job");
    let restore_jobs = directory.path().join("restore-job");
    let handoffs = directory.path().join("handoffs");
    fs::create_dir(&jobs).unwrap();
    fs::create_dir(&restore_jobs).unwrap();
    let store = scale_library(&source, &mode);
    let revision = store.revision().unwrap();
    drop(store);
    let mut db = Connection::open(source.join("persistent/persistent.db")).unwrap();
    let generation: String = serde_json::from_str(
        &db.query_row::<String, _, _>(
            "SELECT value FROM meta WHERE key='activeGeneration'",
            [],
            |r| r.get(0),
        )
        .unwrap(),
    )
    .unwrap();
    let preparation = Instant::now();
    if mode.starts_with("database-") {
        let (character,conversation):(String,String)=db.query_row("SELECT character_id,conversation_id FROM conversations WHERE generation=?1 LIMIT 1",[&generation],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        let transaction = db.transaction().unwrap();
        transaction.execute("DELETE FROM messages WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,character,conversation]).unwrap();
        let data = "synthetic-data-".repeat(74900);
        let messages = if mode == "database-probe" {
            128
        } else {
            1024_i64
        };
        for index in 0..messages {
            let value = serde_json::to_string(
                &serde_json::json!({"role":"user","data":data,"chatId":format!("scale-{index}")}),
            )
            .unwrap();
            transaction
                .execute(
                    "INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        generation,
                        character,
                        conversation,
                        index,
                        format!("scale-{index}"),
                        value
                    ],
                )
                .unwrap();
        }
        transaction.execute("UPDATE conversations SET message_count=?4 WHERE generation=?1 AND character_id=?2 AND conversation_id=?3",params![generation,character,conversation,messages]).unwrap();
        transaction.commit().unwrap();
    } else {
        let transaction = db.transaction().unwrap();
        let count = if mode == "files-100k" {
            100_000_u64
        } else if mode.ends_with("small") {
            1000
        } else {
            1
        };
        for index in 0..count {
            let mut value = vec![0_u8; 1024];
            value[..8].copy_from_slice(&index.to_le_bytes());
            let (hash, bytes) = if mode == "payload-4g" {
                let path = source.join("large-synthetic");
                let mut file = fs::OpenOptions::new()
                    .create_new(true)
                    .read(true)
                    .write(true)
                    .open(&path)
                    .unwrap();
                let bytes = 4 * 1024 * 1024 * 1024_u64 + 17;
                file.set_len(bytes).unwrap();
                let hash = portable_backup::copy_hash(
                    &mut file,
                    &mut std::io::sink(),
                    bytes,
                    &NeverCancelled,
                )
                .unwrap();
                drop(file);
                let destination = source
                    .join("assets-v2/objects")
                    .join(&hash[..2])
                    .join(&hash[2..]);
                fs::create_dir_all(destination.parent().unwrap()).unwrap();
                fs::rename(path, destination).unwrap();
                (hash, bytes)
            } else {
                let hash = hex::encode(Sha256::digest(&value));
                let path = source
                    .join("assets-v2/objects")
                    .join(&hash[..2])
                    .join(&hash[2..]);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, &value).unwrap();
                (hash, value.len() as u64)
            };
            transaction.execute("INSERT INTO asset_aliases(generation,logical_key,object_hash,kind,size,mime,name,ext,inlay_type,width,height,metadata) VALUES(?1,?2,?3,'asset',?4,'application/octet-stream','','',NULL,NULL,NULL,'{}')",params![generation,format!("assets/scale-{index}.bin"),hash,bytes as i64]).unwrap();
            // Two aliases per physical payload exercise deduplication and distinct metadata.
            transaction.execute("INSERT INTO asset_aliases(generation,logical_key,object_hash,kind,size,mime,name,ext,inlay_type,width,height,metadata) VALUES(?1,?2,?3,'asset',?4,'application/octet-stream','','',NULL,NULL,NULL,'{\"second\":true}')",params![generation,format!("assets/second-{index}.bin"),hash,bytes as i64]).unwrap();
        }
        transaction.commit().unwrap();
    }
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    drop(db);
    let source_db_bytes = fs::metadata(source.join("persistent/persistent.db"))
        .unwrap()
        .len();
    if mode == "database-1g" {
        assert!(source_db_bytes >= 1024 * 1024 * 1024);
    }
    println!(
        "SCALE prepared mode={mode} dbBytes={source_db_bytes} milliseconds={}",
        preparation.elapsed().as_millis()
    );
    let store = PersistentStore::open(&source).unwrap();
    let registry = super::super::JobRegistry::default();
    let job = registry
        .create_internal(
            super::super::JobKind::ExportPortableBackup,
            Some(revision),
            vec![],
            false,
        )
        .unwrap();
    let start = Instant::now();
    let exported = measure("export", &job, || {
        export_portable(None, revision, &jobs, &handoffs, store, &job, None)
    })
    .unwrap();
    let export_ms = start.elapsed().as_millis();
    assert!(
        exported.warning_codes.iter().all(|_| false),
        "export warnings: {:?}",
        exported.warning_codes
    );
    let archive_path = std::path::PathBuf::from(exported.handoff_path.unwrap());
    let archive =
        VerifiedArchive::open(File::open(&archive_path).unwrap(), &jobs, &NeverCancelled).unwrap();
    let expected = digest_raw_tables(&archive.db, &NeverCancelled).unwrap();
    if mode == "database-probe" {
        let mut target_store = scale_library(&target, &mode);
        let revision = target_store.revision().unwrap();
        println!("MEMORY beforeStage={:?}", peak_working_set());
        let stage = target_store
            .stage_portable_records(&archive.db, &NeverCancelled)
            .unwrap();
        println!("MEMORY afterStage={:?}", peak_working_set());
        let prepared = target_store
            .prepare_replace_commit(&stage.staging_id, Some(revision))
            .unwrap();
        println!("MEMORY afterPrepare={:?}", peak_working_set());
        let authorized = prepared;
        target_store.finish_prepared_replace(authorized).unwrap();
        println!("MEMORY afterCommit={:?}", peak_working_set());
        return;
    }
    drop(archive);
    let target_store = scale_library(&target, &mode);
    let target_revision = target_store.revision().unwrap();
    let restore_job = registry
        .create_internal(
            super::super::JobKind::RestorePortableBackup,
            Some(target_revision),
            vec![],
            false,
        )
        .unwrap();
    let start = Instant::now();
    let result = measure("restore", &restore_job, || {
        restore_portable(
            OpenedJobSource {
                file: File::open(archive_path).unwrap(),
                total_bytes: exported.source_bytes,
            },
            true,
            target_revision,
            &restore_jobs,
            target_store,
            &restore_job,
            None,
        )
    })
    .unwrap();
    let restore_ms = start.elapsed().as_millis();
    assert!(
        result
            .warning_codes
            .iter()
            .all(|code| code == "expected-missing-reference"),
        "restore warnings: {:?}",
        result.warning_codes
    );
    let mut restored = PersistentStore::open(&target).unwrap();
    let lease = restored.acquire_revision(result.revision).unwrap().lease;
    let mut catalog = Catalog::create(&jobs, "scale-reexport", result.revision).unwrap();
    restored
        .capture_portable_records(&lease, &mut catalog.db, &NeverCancelled)
        .unwrap();
    restored.release_revision(&lease).unwrap();
    assert_eq!(
        expected,
        digest_raw_tables(&catalog.db, &NeverCancelled).unwrap()
    );
    println!("SCALE result mode={mode} sourceDbBytes={source_db_bytes} archiveBytes={} exportMs={export_ms} restoreMs={restore_ms} peakWorkingSetBytes={:?} retainedBytes={}",exported.source_bytes,peak_working_set(),tree_bytes(directory.path()));
}

fn measure<T>(label: &str, job: &JobControl, operation: impl FnOnce() -> T) -> T {
    use std::sync::atomic::{AtomicBool, Ordering};
    let done = AtomicBool::new(false);
    struct Stop<'a>(&'a AtomicBool);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let mut previous = String::new();
            let mut threshold = 0;
            while !done.load(Ordering::Relaxed) {
                let phase = format!("{:?}", job.status().phase);
                let peak = peak_working_set().unwrap_or(0);
                if phase != previous || peak > threshold + 64 * 1024 * 1024 {
                    println!(
                        "SCALE phase operation={label} phase={phase} peakWorkingSetBytes={peak}"
                    );
                    previous = phase;
                    threshold = peak;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        });
        let stop = Stop(&done);
        let result = operation();
        drop(stop);
        println!(
            "SCALE operation={label} finished peakWorkingSetBytes={:?}",
            peak_working_set()
        );
        result
    })
}

fn tree_bytes(path: &Path) -> u64 {
    fs::read_dir(path)
        .unwrap()
        .map(|e| {
            let p = e.unwrap().path();
            let m = fs::symlink_metadata(&p).unwrap();
            if m.is_dir() {
                tree_bytes(&p)
            } else {
                m.len()
            }
        })
        .sum()
}
#[cfg(windows)]
fn peak_working_set() -> Option<usize> {
    #[repr(C)]
    struct Counters {
        cb: u32,
        page_faults: u32,
        peak: usize,
        working: usize,
        quota_peak_paged: usize,
        quota_paged: usize,
        quota_peak_nonpaged: usize,
        quota_nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
    }
    #[link(name = "psapi")]
    unsafe extern "system" {
        fn GetProcessMemoryInfo(
            process: *mut std::ffi::c_void,
            counters: *mut Counters,
            size: u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
    }
    let mut counters: Counters = unsafe { std::mem::zeroed() };
    counters.cb = std::mem::size_of::<Counters>() as u32;
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
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

fn scale_library(root: &Path, mode: &str) -> PersistentStore {
    let store = super::tests::library(root);
    if mode.ends_with("small") {
        // Compare the shared, normally restorable domain. The old reader rejects omitted
        // optional root blocks. Separate portable fixtures preserve those exact omissions.
        let db = Connection::open(root.join("persistent/persistent.db")).unwrap();
        let generation: String = serde_json::from_str(
            &db.query_row::<String, _, _>(
                "SELECT value FROM meta WHERE key='activeGeneration'",
                [],
                |row| row.get(0),
            )
            .unwrap(),
        )
        .unwrap();
        db.execute("UPDATE root SET value=json_set(value,'$.modules',json('[]'),'$.loadouts',json('[]'),'$.plugins',json('[]')) WHERE generation=?1", [&generation]).unwrap();
        db.execute("INSERT INTO plugin_storage(generation,storage_key,byte_size,ordinal,value) VALUES(?1,'synthetic-baseline',2,0,'{}')", [&generation]).unwrap();
    }
    store
}
