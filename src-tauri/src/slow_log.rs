//! Records waits long enough to hold up the app in the native log, so a slow
//! start or sync can be traced to the lock, sync stage or pause behind it.
//! Repeats at one place within a minute become one summary line, so a burst
//! cannot push the rest of the log out.

use std::collections::HashMap;
use std::panic::Location;
use std::sync::{LazyLock, LockResult, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant, SystemTime};

const TARGET: &str = "slow-operation";
const LOCK_THRESHOLD: Duration = Duration::from_secs(1);
const STAGE_THRESHOLD: Duration = Duration::from_secs(5);
const WATCH_TICK: Duration = Duration::from_secs(1);
const WATCH_THRESHOLD: Duration = Duration::from_secs(5);
const REPEAT_WINDOW: Duration = Duration::from_secs(60);

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

/// What a slow record measured, kept to summarize its repeats.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Measure {
    Lock { waited: Duration, held: Duration },
    Stage { took: Duration },
}

impl Measure {
    fn longest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Lock { waited, held }, Self::Lock { waited: other_waited, held: other_held }) => {
                Self::Lock { waited: waited.max(other_waited), held: held.max(other_held) }
            }
            (Self::Stage { took }, Self::Stage { took: other_took }) => Self::Stage { took: took.max(other_took) },
            (_, other) => other,
        }
    }

    fn describe(self) -> String {
        match self {
            Self::Lock { waited, held } => {
                format!("longest wait {} ms, longest hold {} ms", waited.as_millis(), held.as_millis())
            }
            Self::Stage { took } => format!("longest {} ms", took.as_millis()),
        }
    }
}

struct Window {
    opened: Instant,
    repeats: u64,
    longest: Option<Measure>,
}

impl Window {
    fn summary(&self, subject: &str, now: Instant) -> Option<String> {
        let longest = self.longest?;
        let plural = if self.repeats == 1 { "" } else { "s" };
        Some(format!(
            "{subject} was slow {} more time{plural} over {} s, {}",
            self.repeats,
            now.duration_since(self.opened).as_secs(),
            longest.describe(),
        ))
    }
}

/// Holds back repeats of a record about the same subject, so a burst leaves
/// its first line and one summary instead of a line for every wait.
#[derive(Default)]
struct Repeats {
    open: HashMap<String, Window>,
}

impl Repeats {
    /// Returns the lines to record for `line`, a slow `measure` of `subject` seen at `now`.
    fn note(&mut self, subject: String, line: String, measure: Measure, now: Instant) -> Vec<String> {
        let mut lines = Vec::new();
        if let Some(window) = self.open.get_mut(&subject) {
            if now.duration_since(window.opened) < REPEAT_WINDOW {
                window.repeats += 1;
                window.longest = Some(window.longest.map_or(measure, |longest| longest.longest(measure)));
                return lines;
            }
            lines.extend(window.summary(&subject, now));
        }
        lines.push(line);
        self.open.insert(subject, Window { opened: now, repeats: 0, longest: None });
        lines
    }

    /// Closes the windows that ended by `now`, and with `all` also every window
    /// holding repeats, returning a summary for each that held any.
    fn close(&mut self, now: Instant, all: bool) -> Vec<String> {
        let mut lines = Vec::new();
        self.open.retain(|subject, window| {
            let ended = now.duration_since(window.opened) >= REPEAT_WINDOW;
            if !ended && !(all && window.repeats > 0) {
                return true;
            }
            lines.extend(window.summary(subject, now));
            false
        });
        lines
    }
}

static REPEATS: LazyLock<Mutex<Repeats>> = LazyLock::new(|| Mutex::new(Repeats::default()));

fn note(subject: String, line: String, measure: Measure) {
    let lines = REPEATS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .note(subject, line, measure, Instant::now());
    for line in lines {
        record("warn", line);
    }
}

fn close_repeats(all: bool) {
    let lines = REPEATS.lock().unwrap_or_else(PoisonError::into_inner).close(Instant::now(), all);
    for line in lines {
        record("warn", line);
    }
}

/// Records the summary of every burst still being counted, so a log read now
/// shows how large it is.
pub(crate) fn summarize_repeats() {
    close_repeats(true);
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
            let (name, file, line) = (self.name, self.location.file(), self.location.line());
            note(
                format!("{name} lock at {file}:{line}"),
                format!(
                    "{name} lock held {} ms after waiting {} ms at {file}:{line}",
                    held.as_millis(),
                    waited.as_millis(),
                ),
                Measure::Lock { waited, held },
            );
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
/// A wait for a held lock leaves the async worker to other tasks.
#[track_caller]
pub(crate) fn lock<'a, T>(mutex: &'a Mutex<T>, name: &'static str) -> LockResult<TimedGuard<'a, T>> {
    let location = Location::caller();
    let requested = Instant::now();
    let timed = |guard| TimedGuard {
        guard,
        _timing: LockTiming { name, location, requested, acquired: Instant::now() },
    };
    let locked = match mutex.try_lock() {
        Ok(guard) => Ok(guard),
        Err(TryLockError::Poisoned(poisoned)) => Err(poisoned),
        Err(TryLockError::WouldBlock) => crate::async_worker::blocking(|| mutex.lock()),
    };
    match locked {
        Ok(guard) => Ok(timed(guard)),
        Err(poisoned) => Err(PoisonError::new(timed(poisoned.into_inner()))),
    }
}

/// Records `stage` when it ran from `started` for at least the stage threshold.
pub(crate) fn record_stage(area: &str, stage: &str, started: Instant) {
    let took = started.elapsed();
    if took >= threshold(STAGE_THRESHOLD) {
        note(
            format!("{area} {stage}"),
            format!("{area} {stage} took {} ms", took.as_millis()),
            Measure::Stage { took },
        );
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
/// workers were blocked. The thread also writes the summaries of ended bursts.
pub(crate) fn start_stall_watch() {
    let thread = std::thread::Builder::new().name("stall-watch".to_owned()).spawn(|| {
        let mut watch = GapWatch::new(SystemTime::now(), WATCH_TICK, WATCH_THRESHOLD);
        loop {
            std::thread::sleep(WATCH_TICK);
            if let Some(missed) = watch.observe(SystemTime::now()) {
                record("info", format!("app threads did not run for {} ms", missed.as_millis()));
            }
            close_repeats(false);
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

    #[test]
    fn repeats_of_a_subject_leave_its_first_line_and_one_summary_per_window() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let window = REPEAT_WINDOW.as_millis() as u64;
        let lock = |waited: u64, held: u64| Measure::Lock {
            waited: Duration::from_millis(waited),
            held: Duration::from_millis(held),
        };
        let mut repeats = Repeats::default();
        let mut note = |subject: &str, line: &str, measure, now| repeats.note(subject.into(), line.into(), measure, now);
        assert_eq!(note("job store", "first", lock(2120, 6), at(0)), ["first"]);
        assert!(note("job store", "second", lock(2439, 5), at(6)).is_empty());
        assert!(note("job store", "third", lock(2200, 7), at(12)).is_empty());
        assert_eq!(note("pull", "pull took", Measure::Stage { took: Duration::from_secs(6) }, at(20)), ["pull took"]);
        // A record after its window ended first summarizes that window.
        assert_eq!(
            note("job store", "fourth", lock(1100, 6), at(window + 1_000)),
            ["job store was slow 2 more times over 61 s, longest wait 2439 ms, longest hold 7 ms", "fourth"],
        );
        assert!(note("job store", "fifth", lock(1300, 6), at(window + 2_000)).is_empty());
        // A window without repeats closes without a line, and its next record is written in full.
        assert!(repeats.close(at(window + 20), false).is_empty());
        assert_eq!(
            repeats.note("pull".into(), "pull again".into(), Measure::Stage { took: Duration::from_secs(7) }, at(window + 30)),
            ["pull again"],
        );
        // Closing every window early summarizes only those holding repeats.
        assert_eq!(
            repeats.close(at(window + 3_000), true),
            ["job store was slow 1 more time over 2 s, longest wait 1300 ms, longest hold 6 ms"],
        );
        assert!(repeats.note("pull".into(), "pull once more".into(), Measure::Stage { took: Duration::from_secs(5) }, at(window + 3_100)).is_empty());
    }

    #[test]
    fn a_wait_for_a_held_lock_leaves_the_async_worker_to_other_tasks() {
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(1).build().unwrap();
        let mutex = std::sync::Arc::new(Mutex::new(0));
        let held = mutex.lock().unwrap();
        let (started_sent, started) = std::sync::mpsc::channel();
        let waiting = std::sync::Arc::clone(&mutex);
        let waiter = runtime.spawn(async move {
            started_sent.send(()).unwrap();
            *lock(&waiting, "synthetic").unwrap() + 1
        });
        // The waiter is on the only worker, so another task runs only if the wait gives it up.
        // A blocked worker also stops the runtime's timers, so the test waits on a plain channel.
        started.recv().unwrap();
        let (ran_sent, ran) = std::sync::mpsc::channel();
        runtime.spawn(async move { let _ = ran_sent.send(7); });
        let other = ran.recv_timeout(Duration::from_secs(5));
        drop(held);
        assert_eq!(other, Ok(7));
        assert_eq!(runtime.block_on(waiter).unwrap(), 1);
    }

    #[test]
    fn reading_the_log_summarizes_a_burst_still_being_counted() {
        let app = tauri::test::mock_builder()
            .build(tauri::test::mock_context(tauri::test::noop_assets()))
            .unwrap();
        tauri::Manager::manage(&app, crate::native_log::global_state());
        let subject = "synthetic burst-stage";
        let lines = |entries: Vec<crate::native_log::LogEntry>| {
            entries.into_iter().map(|entry| entry.message).filter(|message| message.starts_with(subject)).collect::<Vec<_>>()
        };
        let started = Instant::now();
        with_test_threshold(Duration::ZERO, || {
            for _ in 0..3 {
                record_stage("synthetic", "burst-stage", started);
            }
        });
        assert_eq!(lines(crate::native_log::global_state().tail(None)).len(), 1);
        let read = lines(crate::native_log::native_log_tail(tauri::Manager::state(&app), None));
        assert_eq!(read.len(), 2, "{read:?}");
        assert!(read[1].starts_with(&format!("{subject} was slow 2 more times over ")), "{read:?}");
    }
}
