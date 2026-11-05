//! Records waits long enough to hold up the app in the native log, so a slow
//! start or sync can be traced to the lock, sync stage or pause behind it.

use std::panic::Location;
use std::sync::{LockResult, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime};

const TARGET: &str = "slow-operation";
const LOCK_THRESHOLD: Duration = Duration::from_secs(1);
const STAGE_THRESHOLD: Duration = Duration::from_secs(5);
const WATCH_TICK: Duration = Duration::from_secs(1);
const WATCH_THRESHOLD: Duration = Duration::from_secs(5);

#[cfg(test)]
thread_local! {
    static TEST_THRESHOLD: std::cell::Cell<Option<Duration>> = const { std::cell::Cell::new(None) };
}

fn threshold(default: Duration) -> Duration {
    #[cfg(test)]
    if let Some(value) = TEST_THRESHOLD.with(|value| value.get()) {
        return value;
    }
    default
}

/// Lowers the lock and stage thresholds for work run on this thread.
#[cfg(test)]
pub(crate) fn with_test_threshold<T>(value: Duration, run: impl FnOnce() -> T) -> T {
    TEST_THRESHOLD.with(|threshold| threshold.set(Some(value)));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(run));
    TEST_THRESHOLD.with(|threshold| threshold.set(None));
    result.unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

fn record(level: &str, message: String) {
    crate::native_log::log_global(level, TARGET, message);
}

/// A lock guard that, once released, records a wait or hold at or above the threshold.
pub(crate) struct TimedGuard<'a, T> {
    // Declared first so the lock is released before the timing records.
    guard: MutexGuard<'a, T>,
    _timing: LockTiming,
}

struct LockTiming {
    name: &'static str,
    location: &'static Location<'static>,
    requested: Instant,
    acquired: Instant,
}

impl Drop for LockTiming {
    fn drop(&mut self) {
        let waited = self.acquired.duration_since(self.requested);
        let held = self.acquired.elapsed();
        let threshold = threshold(LOCK_THRESHOLD);
        if waited >= threshold || held >= threshold {
            record("warn", format!(
                "{} lock held {} ms after waiting {} ms at {}:{}",
                self.name, held.as_millis(), waited.as_millis(), self.location.file(), self.location.line(),
            ));
        }
    }
}

impl<T> std::ops::Deref for TimedGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> std::ops::DerefMut for TimedGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

/// Locks `mutex` like `Mutex::lock`, naming the caller in the record of a long wait or hold.
#[track_caller]
pub(crate) fn lock<'a, T>(mutex: &'a Mutex<T>, name: &'static str) -> LockResult<TimedGuard<'a, T>> {
    let location = Location::caller();
    let requested = Instant::now();
    let timed = |guard| TimedGuard {
        guard,
        _timing: LockTiming { name, location, requested, acquired: Instant::now() },
    };
    match mutex.lock() {
        Ok(guard) => Ok(timed(guard)),
        Err(poisoned) => Err(PoisonError::new(timed(poisoned.into_inner()))),
    }
}

/// Records `stage` when it ran from `started` for at least the stage threshold.
pub(crate) fn record_stage(area: &str, stage: &str, started: Instant) {
    let elapsed = started.elapsed();
    if elapsed >= threshold(STAGE_THRESHOLD) {
        record("warn", format!("{area} {stage} took {} ms", elapsed.as_millis()));
    }
}

/// Notices a loop that wakes every `tick` falling behind by at least `threshold`,
/// as it does while the app is suspended or the threads it runs on are blocked.
struct GapWatch {
    last: SystemTime,
    tick: Duration,
    threshold: Duration,
}

impl GapWatch {
    fn new(now: SystemTime, tick: Duration, threshold: Duration) -> Self {
        Self { last: now, tick, threshold }
    }

    /// Returns how long the loop did not run, when that reaches the threshold.
    fn observe(&mut self, now: SystemTime) -> Option<Duration> {
        let elapsed = now.duration_since(self.last).unwrap_or_default();
        self.last = now;
        let missed = elapsed.saturating_sub(self.tick);
        (missed >= self.threshold).then_some(missed)
    }
}

/// Watches from a plain thread and from the async runtime. A pause seen by both
/// means the app was not running; one seen only by the runtime means its
/// workers were blocked.
pub(crate) fn start_stall_watch() {
    let thread = std::thread::Builder::new().name("stall-watch".to_owned()).spawn(|| {
        let mut watch = GapWatch::new(SystemTime::now(), WATCH_TICK, WATCH_THRESHOLD);
        loop {
            std::thread::sleep(WATCH_TICK);
            if let Some(missed) = watch.observe(SystemTime::now()) {
                record("info", format!("app threads did not run for {} ms", missed.as_millis()));
            }
        }
    });
    if let Err(error) = thread {
        record("warn", format!("stall watch did not start: {error}"));
    }
    tauri::async_runtime::spawn(async {
        let mut watch = GapWatch::new(SystemTime::now(), WATCH_TICK, WATCH_THRESHOLD);
        loop {
            tokio::time::sleep(WATCH_TICK).await;
            if let Some(missed) = watch.observe(SystemTime::now()) {
                record("info", format!("async runtime did not run for {} ms", missed.as_millis()));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_watch_reports_only_a_gap_that_reaches_the_threshold_past_its_tick() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let tick = Duration::from_secs(1);
        let mut watch = GapWatch::new(start, tick, Duration::from_secs(5));
        assert_eq!(watch.observe(start + Duration::from_millis(5_900)), None);
        assert_eq!(watch.observe(start + Duration::from_millis(12_900)), Some(Duration::from_secs(6)));
        assert_eq!(watch.observe(start + Duration::from_millis(13_900)), None);
        // A clock set back is not a pause, and the next gap is measured from it.
        assert_eq!(watch.observe(start), None);
        assert_eq!(watch.observe(start + Duration::from_secs(1)), None);
    }
}
