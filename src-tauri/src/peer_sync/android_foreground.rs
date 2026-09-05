use crate::local_backup::CancellationProbe;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AndroidForegroundLane {
    DeviceSyncSource,
    P4Target,
    P5Target,
}

impl AndroidForegroundLane {
    // Android JNI parses lane strings from the Kotlin service; desktop builds
    // only route lanes through serde.
    #[cfg_attr(all(desktop, not(test)), allow(dead_code))]
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "device-sync-source" => Some(Self::DeviceSyncSource),
            "p4-target" => Some(Self::P4Target),
            "p5-target" => Some(Self::P5Target),
            _ => None,
        }
    }

    pub(crate) fn is_source(self) -> bool {
        matches!(self, Self::DeviceSyncSource)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AndroidForegroundKey {
    pub(crate) lane: AndroidForegroundLane,
    pub(crate) operation_id: String,
    pub(crate) generation: u64,
}

#[derive(Clone)]
pub(crate) struct AndroidCancellationProbe(Arc<AtomicBool>);

impl AndroidCancellationProbe {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl CancellationProbe for AndroidCancellationProbe {
    fn is_cancelled(&self) -> bool {
        self.is_cancelled()
    }
}

// The foreground registry backs the Android service lifecycle; desktop builds
// compile it only for tests.
#[cfg_attr(all(desktop, not(test)), allow(dead_code))]
type StopCallback = Box<dyn FnOnce() + Send + 'static>;

#[cfg_attr(all(desktop, not(test)), allow(dead_code))]
struct ForegroundEntry {
    key: AndroidForegroundKey,
    attached: bool,
    target_retained: bool,
    cancellation: Arc<AtomicBool>,
    source_stop: Option<StopCallback>,
}

#[cfg_attr(all(desktop, not(test)), allow(dead_code))]
#[derive(Default)]
pub(crate) struct AndroidForegroundRegistry {
    next_generation: AtomicU64,
    entry: Mutex<Option<ForegroundEntry>>,
}

#[cfg_attr(all(desktop, not(test)), allow(dead_code))]
impl AndroidForegroundRegistry {
    pub(crate) fn reserve(
        &self,
        lane: AndroidForegroundLane,
    ) -> Result<AndroidForegroundKey, String> {
        let generation = self.next_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let key = AndroidForegroundKey {
            lane,
            operation_id: uuid::Uuid::new_v4().to_string(),
            generation,
        };
        let mut entry = self
            .entry
            .lock()
            .map_err(|_| "Android foreground registry is unavailable".to_owned())?;
        if entry.is_some() {
            return Err("Android foreground service is already reserved".to_owned());
        }
        *entry = Some(ForegroundEntry {
            key: key.clone(),
            attached: false,
            target_retained: false,
            cancellation: Arc::new(AtomicBool::new(false)),
            source_stop: None,
        });
        Ok(key)
    }

    pub(crate) fn attach_exact(&self, key: &AndroidForegroundKey) -> bool {
        let Ok(mut entry) = self.entry.lock() else {
            return false;
        };
        let Some(entry) = entry.as_mut() else {
            return false;
        };
        if entry.key != *key {
            return false;
        }
        entry.attached = true;
        true
    }

    pub(crate) fn acquire_exact(
        &self,
        key: &AndroidForegroundKey,
    ) -> Option<AndroidCancellationProbe> {
        let entry = self.entry.lock().ok()?;
        let entry = entry.as_ref()?;
        (entry.attached && entry.key == *key && !entry.cancellation.load(Ordering::SeqCst))
            .then(|| AndroidCancellationProbe(Arc::clone(&entry.cancellation)))
    }

    pub(crate) fn set_source_stop_callback_exact(
        &self,
        key: &AndroidForegroundKey,
        callback: impl FnOnce() + Send + 'static,
    ) -> bool {
        // Stop callbacks are a source-lane contract: cancel_exact runs them
        // synchronously on the caller's thread, and the target-lane cancel
        // paths hold command-state locks that those callbacks re-acquire.
        // Rejecting target-lane registrations keeps that re-entrancy
        // impossible.
        if !key.lane.is_source() {
            return false;
        }
        let Ok(mut entry) = self.entry.lock() else {
            return false;
        };
        let Some(entry) = entry.as_mut() else {
            return false;
        };
        if !entry.attached || entry.key != *key || entry.cancellation.load(Ordering::SeqCst) {
            return false;
        }
        entry.source_stop = Some(Box::new(callback));
        true
    }

    pub(crate) fn cancel_exact(&self, key: &AndroidForegroundKey) -> bool {
        let callback = {
            let Ok(mut entry) = self.entry.lock() else {
                return false;
            };
            let Some(entry) = entry.as_mut() else {
                return false;
            };
            if entry.key != *key {
                return false;
            }
            entry.cancellation.store(true, Ordering::SeqCst);
            entry.source_stop.take()
        };
        if let Some(callback) = callback {
            callback();
        }
        true
    }

    pub(crate) fn abandon_source_exact(&self, key: &AndroidForegroundKey) -> bool {
        if !key.lane.is_source() {
            return false;
        }
        let callback = {
            let Ok(mut entry) = self.entry.lock() else {
                return false;
            };
            let Some(current) = entry.as_mut() else {
                return true;
            };
            if current.key != *key {
                return false;
            }
            current.cancellation.store(true, Ordering::SeqCst);
            let callback = current.source_stop.take();
            *entry = None;
            callback
        };
        if let Some(callback) = callback {
            callback();
        }
        true
    }

    pub(crate) fn source_status(
        &self,
        lane: AndroidForegroundLane,
    ) -> Option<AndroidForegroundKey> {
        if !lane.is_source() {
            return None;
        }
        self.entry
            .lock()
            .ok()?
            .as_ref()
            .filter(|entry| entry.key.lane == lane)
            .map(|entry| entry.key.clone())
    }

    pub(crate) fn retain_target_exact(&self, key: &AndroidForegroundKey) -> bool {
        let Ok(mut entry) = self.entry.lock() else {
            return false;
        };
        let Some(entry) = entry.as_mut() else {
            return false;
        };
        if !matches!(
            key.lane,
            AndroidForegroundLane::P4Target | AndroidForegroundLane::P5Target
        ) || entry.key != *key
            || !entry.attached
            || entry.cancellation.load(Ordering::SeqCst)
        {
            return false;
        }
        entry.target_retained = true;
        true
    }

    pub(crate) fn release_target_exact(&self, key: &AndroidForegroundKey) -> bool {
        if !matches!(
            key.lane,
            AndroidForegroundLane::P4Target | AndroidForegroundLane::P5Target
        ) {
            return false;
        }
        let callback = {
            let Ok(mut entry) = self.entry.lock() else {
                return false;
            };
            let Some(current) = entry.as_mut() else {
                return true;
            };
            if current.key != *key {
                return false;
            }
            current.cancellation.store(true, Ordering::SeqCst);
            let callback = current.source_stop.take();
            *entry = None;
            callback
        };
        if let Some(callback) = callback {
            callback();
        }
        true
    }

    pub(crate) fn detach_if_generation(&self, key: &AndroidForegroundKey) -> bool {
        let Ok(mut entry) = self.entry.lock() else {
            return false;
        };
        if entry
            .as_ref()
            .is_none_or(|entry| entry.key != *key || entry.target_retained)
        {
            return false;
        }
        *entry = None;
        true
    }
}

#[cfg_attr(all(desktop, not(test)), allow(dead_code))]
pub(crate) fn registry() -> &'static AndroidForegroundRegistry {
    static REGISTRY: OnceLock<AndroidForegroundRegistry> = OnceLock::new();
    REGISTRY.get_or_init(AndroidForegroundRegistry::default)
}

// Shared attach-wait for Android foreground lanes. Async so command bodies
// never block a tokio worker thread while polling for service attach.
#[cfg(any(target_os = "android", test))]
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
pub(crate) async fn acquire_foreground_lane(
    key: &AndroidForegroundKey,
    lane: AndroidForegroundLane,
) -> Result<AndroidCancellationProbe, String> {
    const SERVICE_ATTACH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    if key.lane != lane {
        return Err("Android foreground lane is not allowed".to_owned());
    }
    let deadline = std::time::Instant::now() + SERVICE_ATTACH_TIMEOUT;
    loop {
        if let Some(cancellation) = registry().acquire_exact(key) {
            return Ok(cancellation);
        }
        if std::time::Instant::now() >= deadline {
            return Err("Android foreground service did not attach".to_owned());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

#[cfg(test)]
pub(crate) fn test_registry_guard() -> std::sync::MutexGuard<'static, ()> {
    static TEST_REGISTRY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    TEST_REGISTRY_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[tauri::command]
#[cfg(target_os = "android")]
pub(crate) fn peer_sync_foreground_source_abandon(
    foreground: AndroidForegroundKey,
) -> Result<bool, String> {
    if !foreground.lane.is_source() {
        return Err("Android foreground identity is not a source lane".to_owned());
    }
    Ok(registry().abandon_source_exact(&foreground))
}

#[tauri::command]
#[cfg(target_os = "android")]
pub(crate) fn peer_sync_foreground_source_status() -> Option<AndroidForegroundKey> {
    registry().source_status(AndroidForegroundLane::DeviceSyncSource)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[test]
    fn only_user_started_peer_sync_lanes_are_allowed() {
        assert_eq!(
            AndroidForegroundLane::parse("device-sync-source"),
            Some(AndroidForegroundLane::DeviceSyncSource)
        );
        assert_eq!(
            AndroidForegroundLane::parse("p4-target"),
            Some(AndroidForegroundLane::P4Target)
        );
        assert_eq!(
            AndroidForegroundLane::parse("p5-target"),
            Some(AndroidForegroundLane::P5Target)
        );
        assert_eq!(AndroidForegroundLane::parse("p1-source"), None);
        assert_eq!(AndroidForegroundLane::parse("p4-source"), None);
        assert_eq!(AndroidForegroundLane::parse("p5-source"), None);
        assert_eq!(AndroidForegroundLane::parse("p3-target"), None);
        assert_eq!(AndroidForegroundLane::parse("quick-tunnel"), None);
    }

    #[test]
    fn attach_acquire_cancel_and_detach_are_generation_exact() {
        let registry = AndroidForegroundRegistry::default();
        let old = registry
            .reserve(AndroidForegroundLane::DeviceSyncSource)
            .unwrap();
        assert!(registry.detach_if_generation(&old));
        let current = registry
            .reserve(AndroidForegroundLane::DeviceSyncSource)
            .unwrap();
        assert!(current.generation > old.generation);
        assert!(!registry.attach_exact(&old));
        assert!(registry.attach_exact(&current));
        let cancellation = registry.acquire_exact(&current).unwrap();
        assert!(!cancellation.is_cancelled());
        assert!(!registry.cancel_exact(&old));
        assert!(registry.cancel_exact(&current));
        assert!(cancellation.is_cancelled());
        assert!(registry.acquire_exact(&current).is_none());
        assert!(!registry.detach_if_generation(&old));
        assert!(registry.detach_if_generation(&current));
    }

    #[test]
    fn source_stop_callback_runs_outside_registry_mutex_once() {
        let registry = Arc::new(AndroidForegroundRegistry::default());
        let key = registry
            .reserve(AndroidForegroundLane::DeviceSyncSource)
            .unwrap();
        assert!(registry.attach_exact(&key));
        let called = Arc::new(AtomicBool::new(false));
        let callback_registry = Arc::clone(&registry);
        let callback_key = key.clone();
        let callback_called = Arc::clone(&called);
        assert!(registry.set_source_stop_callback_exact(&key, move || {
            assert!(callback_registry.attach_exact(&callback_key));
            callback_called.store(true, Ordering::SeqCst);
        }));
        assert!(registry.cancel_exact(&key));
        assert!(called.load(Ordering::SeqCst));
        assert!(registry.cancel_exact(&key));
        assert!(called.load(Ordering::SeqCst));
    }

    #[test]
    fn source_stop_callbacks_are_rejected_for_target_lanes() {
        let registry = AndroidForegroundRegistry::default();
        let key = registry.reserve(AndroidForegroundLane::P4Target).unwrap();
        assert!(registry.attach_exact(&key));
        assert!(!registry.set_source_stop_callback_exact(&key, || {}));
        assert!(registry.cancel_exact(&key));
    }

    #[test]
    fn foreground_owner_is_globally_exclusive_across_reserved_and_attached_lanes() {
        let registry = AndroidForegroundRegistry::default();
        let source = registry
            .reserve(AndroidForegroundLane::DeviceSyncSource)
            .unwrap();
        assert!(registry.reserve(AndroidForegroundLane::P4Target).is_err());
        assert!(registry.attach_exact(&source));
        assert!(registry.reserve(AndroidForegroundLane::P4Target).is_err());
        assert!(registry.cancel_exact(&source));
        assert!(registry.reserve(AndroidForegroundLane::P4Target).is_err());
        assert!(registry.detach_if_generation(&source));

        let target = registry.reserve(AndroidForegroundLane::P4Target).unwrap();
        assert!(registry.attach_exact(&target));
        assert!(!registry.detach_if_generation(&source));
        assert!(registry.acquire_exact(&target).is_some());
        assert!(registry.detach_if_generation(&target));
    }

    #[test]
    fn source_abandon_is_exact_idempotent_and_allows_a_different_lane() {
        let registry = AndroidForegroundRegistry::default();
        let source = registry
            .reserve(AndroidForegroundLane::DeviceSyncSource)
            .unwrap();
        let mut stale = source.clone();
        stale.generation += 1;
        assert!(!registry.abandon_source_exact(&stale));
        assert!(registry.abandon_source_exact(&source));
        assert!(registry.abandon_source_exact(&source));
        let target = registry.reserve(AndroidForegroundLane::P4Target).unwrap();
        // Target lanes are released through release_target_exact, never through
        // the source abandon path.
        assert!(!registry.abandon_source_exact(&target));
        assert!(!registry.abandon_source_exact(&source));
        assert!(registry.detach_if_generation(&target));
    }

    #[test]
    fn source_status_returns_only_the_requested_source_lane() {
        let registry = AndroidForegroundRegistry::default();
        let source = registry
            .reserve(AndroidForegroundLane::DeviceSyncSource)
            .unwrap();
        assert_eq!(
            registry.source_status(AndroidForegroundLane::DeviceSyncSource),
            Some(source.clone())
        );
        assert_eq!(
            registry.source_status(AndroidForegroundLane::P4Target),
            None
        );
        assert_eq!(
            registry.source_status(AndroidForegroundLane::P5Target),
            None
        );
        assert!(registry.abandon_source_exact(&source));
        assert_eq!(
            registry.source_status(AndroidForegroundLane::DeviceSyncSource),
            None
        );
    }

    #[test]
    fn retained_running_target_ignores_service_detach_until_native_release() {
        let registry = AndroidForegroundRegistry::default();
        let target = registry.reserve(AndroidForegroundLane::P4Target).unwrap();
        assert!(registry.attach_exact(&target));
        assert!(registry.retain_target_exact(&target));
        assert!(registry.cancel_exact(&target));
        assert!(!registry.detach_if_generation(&target));
        assert!(registry
            .reserve(AndroidForegroundLane::DeviceSyncSource)
            .is_err());
        assert!(registry.release_target_exact(&target));
        let source = registry
            .reserve(AndroidForegroundLane::DeviceSyncSource)
            .unwrap();
        assert!(!registry.release_target_exact(&target));
        assert!(registry.detach_if_generation(&source));
    }
}
