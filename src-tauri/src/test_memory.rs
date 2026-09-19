use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

#[derive(Debug)]
pub(crate) struct MemoryMeasurement<T> {
    pub value: T,
    pub baseline_working_set_bytes: Option<usize>,
    pub peak_working_set_bytes: Option<usize>,
    pub retained_working_set_bytes: Option<usize>,
}

pub(crate) fn measure_working_set<T>(operation: impl FnOnce() -> T) -> MemoryMeasurement<T> {
    let baseline = current_working_set();
    let running = Arc::new(AtomicBool::new(true));
    let peak = Arc::new(AtomicUsize::new(baseline.unwrap_or(0)));
    let sampler_running = Arc::clone(&running);
    let sampler_peak = Arc::clone(&peak);
    let sampler = std::thread::spawn(move || {
        while sampler_running.load(Ordering::Acquire) {
            if let Some(current) = current_working_set() {
                sampler_peak.fetch_max(current, Ordering::AcqRel);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    });
    let value = operation();
    if let Some(current) = current_working_set() {
        peak.fetch_max(current, Ordering::AcqRel);
    }
    running.store(false, Ordering::Release);
    sampler.join().expect("join memory sampler");
    MemoryMeasurement {
        value,
        baseline_working_set_bytes: baseline,
        peak_working_set_bytes: baseline.map(|_| peak.load(Ordering::Acquire)),
        retained_working_set_bytes: current_working_set(),
    }
}

#[cfg(windows)]
fn current_working_set() -> Option<usize> {
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
fn current_working_set() -> Option<usize> {
    None
}
