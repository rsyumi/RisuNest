use super::*;
use crate::persistent_store::{
    device_store::{
        hypa::HypaEmbeddingWrite,
        plugin_values::PluginDeviceMutation,
        sections::{reset_section_resource_evidence, section_resource_evidence},
    },
    PersistentStore,
};
use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

#[test]
#[ignore = "manual E4 resource evidence at 1k and 100k rows"]
fn e4_section_capture_scale() {
    let rows: usize = std::env::var("RISUNEST_SECTION_SCALE_ROWS")
        .expect("set RISUNEST_SECTION_SCALE_ROWS to 1000 or 100000")
        .parse()
        .expect("row count is numeric");
    assert!(matches!(rows, 1_000 | 100_000));
    let root = tempfile::tempdir().expect("create store root");
    let spool = tempfile::tempdir().expect("create section spool");
    let mut store = PersistentStore::open(root.path()).expect("open persistent store");
    {
        let device = store.device_store_mut().expect("open device store");
        let plugin_owner = "o".repeat(512);
        device.set_section_participating(Section::Hypa, true).unwrap();
        device.set_section_participating(Section::LocalPlugins, true).unwrap();
        device.write_hypa_embeddings(&[HypaEmbeddingWrite {
            cache_key: "f".repeat(64),
            producer: "scale".into(),
            model: "max-vector".into(),
            endpoint: None,
            preprocess_version: 1,
            dimensions: 16_384,
            vector: vec![7; 16_384 * 4],
            metadata: None,
        }]).unwrap();
        for first in (0..rows).step_by(1_024) {
            let end = (first + 1_024).min(rows);
            let mutations = (first..end).map(|index| {
                let value_bytes = if index % 10_000 == 0 {
                    4 * 1_024
                } else if index % 257 == 0 {
                    512
                } else {
                    32 + index % 96
                };
                PluginDeviceMutation::Set {
                    space: if index % 2 == 0 { "string".into() } else { "json".into() },
                    key: format!("scale-{index:06}-{}", "유".repeat(index % 7)),
                    value: if index % 2 == 0 {
                        "v".repeat(value_bytes)
                    } else {
                        format!("\"{}\"", "v".repeat(value_bytes.saturating_sub(2)))
                    },
                }
            }).collect::<Vec<_>>();
            device.write_plugin_device_values(&plugin_owner, &mutations).unwrap();
        }
    }

    let baseline = working_set().unwrap_or(0);
    let started = Instant::now();
    let ((captured, publications), peak) = measure_working_set(|| capture_state_sections(
        &mut store,
        &Sequence::from(1u64),
        &BTreeMap::new(),
        "scale-connection",
        "scale-library",
        spool.path(),
        &Cancellation::default(),
    ).expect("capture scale sections"));
    let elapsed = started.elapsed();
    let plugin = captured.iter().find(|section| section.kind == SectionKind::LocalPlugins).unwrap();
    let hypa = captured.iter().find(|section| section.kind == SectionKind::Hypa).unwrap();
    let plugin_entries = plugin.sources.iter()
        .filter(|source| source.kind == wire::CatalogEntryKind::SectionEntry).count();
    let hypa_objects = hypa.sources.iter()
        .filter(|source| source.kind == wire::CatalogEntryKind::SectionObject).count();
    assert_eq!(plugin_entries, rows);
    assert_eq!(hypa_objects, 1);
    assert_eq!(publications.len(), 2);
    let source_index_bytes = captured.iter().map(|section| {
        std::mem::size_of::<SectionSource>() * section.sources.capacity()
            + section.sources.iter().map(|source| {
                source.key.capacity()
                    + source.content_sha256.capacity()
                    + source.path.to_string_lossy().len()
            }).sum::<usize>()
    }).sum::<usize>();
    assert_eq!(publications.iter().map(|publication| {
        let (connection, _) = open_publication_index(&publication.publication_index_path).unwrap();
        connection.query_row::<i64, _, _>(
            "SELECT count(*) FROM publication_rows", [], |row| row.get(0),
        ).unwrap()
    }).sum::<i64>(), i64::try_from(rows + 1).unwrap());
    drop(publications);
    assert_eq!(load_prepared_section_publications(spool.path()).unwrap().len(), 2);
    let spool_bytes = tree_bytes(spool.path());
    let peak_delta = peak.saturating_sub(baseline);

    drop(captured);
    reset_section_resource_evidence();
    let backup_baseline = working_set().unwrap_or(0);
    let (backup_rows, backup_peak) = measure_working_set(|| {
        let prepared = store.device_store_mut().unwrap().capture_backup_sections(&[
            SectionKind::Hypa,
            SectionKind::LocalPlugins,
        ]).unwrap();
        let mut count = 0usize;
        for section in &prepared {
            section.visit_entries(|_, _| {
                count += 1;
                Ok(())
            }).unwrap();
        }
        count
    });
    let (max_page_rows, max_page_bytes, max_value_workspace_bytes) =
        section_resource_evidence();
    assert_eq!(backup_rows, rows + 1);
    assert!(max_page_rows <= 256);
    assert!(max_page_bytes <= 4 * 1024 * 1024);
    assert_eq!(max_value_workspace_bytes, 64 * 1024);
    println!(
        "SECTION_SCALE rows={rows} maxValueBytes={} entrySources={} sourceIndexEstimatedBytes={source_index_bytes} spoolBytes={spool_bytes} baselineWorkingSetBytes={baseline} peakWorkingSetBytes={peak} peakDeltaBytes={peak_delta} backupBaselineWorkingSetBytes={backup_baseline} backupPeakWorkingSetBytes={backup_peak} backupPeakDeltaBytes={} maxPageRows={max_page_rows} maxPageBytes={max_page_bytes} maxValueWorkspaceBytes={max_value_workspace_bytes} elapsedMs={}",
        64 * 1_024,
        plugin_entries + 1,
        backup_peak.saturating_sub(backup_baseline),
        elapsed.as_millis(),
    );
    assert!(peak_delta < 512 * 1024 * 1024, "section capture exceeded the 512 MiB evidence ceiling");
}

fn tree_bytes(path: &Path) -> u64 {
    fs::read_dir(path).unwrap().map(|entry| {
        let path = entry.unwrap().path();
        let metadata = fs::symlink_metadata(&path).unwrap();
        if metadata.is_dir() { tree_bytes(&path) } else { metadata.len() }
    }).sum()
}

fn measure_working_set<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    let done = Arc::new(AtomicBool::new(false));
    let peak = Arc::new(AtomicUsize::new(working_set().unwrap_or(0)));
    let sampler_done = Arc::clone(&done);
    let sampler_peak = Arc::clone(&peak);
    let sampler = std::thread::spawn(move || {
        while !sampler_done.load(Ordering::Relaxed) {
            if let Some(current) = working_set() {
                sampler_peak.fetch_max(current, Ordering::Relaxed);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    let result = operation();
    done.store(true, Ordering::Relaxed);
    sampler.join().unwrap();
    (result, peak.load(Ordering::Relaxed))
}

#[cfg(windows)]
fn working_set() -> Option<usize> {
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
    (ok != 0).then_some(counters.working)
}

#[cfg(not(windows))]
fn working_set() -> Option<usize> {
    None
}
