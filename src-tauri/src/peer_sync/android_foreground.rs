use crate::local_backup::CancellationProbe;
use serde::{Deserialize, Serialize};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, OnceLock,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AndroidForegroundLane {
    P1Source,
    P4Source,
    P4Target,
}

impl AndroidForegroundLane {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "p1-source" => Some(Self::P1Source),
            "p4-source" => Some(Self::P4Source),
            "p4-target" => Some(Self::P4Target),
            _ => None,
        }
    }

    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::P1Source => "p1-source",
            Self::P4Source => "p4-source",
            Self::P4Target => "p4-target",
        }
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

type StopCallback = Box<dyn FnOnce() + Send + 'static>;

struct ForegroundEntry {
    key: AndroidForegroundKey,
    attached: bool,
    target_retained: bool,
    cancellation: Arc<AtomicBool>,
    source_stop: Option<StopCallback>,
}

#[derive(Default)]
pub(crate) struct AndroidForegroundRegistry {
    next_generation: AtomicU64,
    entry: Mutex<Option<ForegroundEntry>>,
}

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
        if !matches!(
            key.lane,
            AndroidForegroundLane::P1Source | AndroidForegroundLane::P4Source
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

    pub(crate) fn retain_target_exact(&self, key: &AndroidForegroundKey) -> bool {
        let Ok(mut entry) = self.entry.lock() else {
            return false;
        };
        let Some(entry) = entry.as_mut() else {
            return false;
        };
        if key.lane != AndroidForegroundLane::P4Target
            || entry.key != *key
            || !entry.attached
            || entry.cancellation.load(Ordering::SeqCst)
        {
            return false;
        }
        entry.target_retained = true;
        true
    }

    pub(crate) fn release_target_exact(&self, key: &AndroidForegroundKey) -> bool {
        if key.lane != AndroidForegroundLane::P4Target {
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

pub(crate) fn registry() -> &'static AndroidForegroundRegistry {
    static REGISTRY: OnceLock<AndroidForegroundRegistry> = OnceLock::new();
    REGISTRY.get_or_init(AndroidForegroundRegistry::default)
}

#[tauri::command]
#[cfg(target_os = "android")]
pub(crate) fn peer_sync_foreground_source_abandon(
    foreground: AndroidForegroundKey,
) -> Result<bool, String> {
    if !matches!(
        foreground.lane,
        AndroidForegroundLane::P1Source | AndroidForegroundLane::P4Source
    ) {
        return Err("Android foreground identity is not a source lane".to_owned());
    }
    Ok(registry().abandon_source_exact(&foreground))
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
            AndroidForegroundLane::parse("p1-source"),
            Some(AndroidForegroundLane::P1Source)
        );
        assert_eq!(
            AndroidForegroundLane::parse("p4-source"),
            Some(AndroidForegroundLane::P4Source)
        );
        assert_eq!(
            AndroidForegroundLane::parse("p4-target"),
            Some(AndroidForegroundLane::P4Target)
        );
        assert_eq!(AndroidForegroundLane::parse("p3-target"), None);
        assert_eq!(AndroidForegroundLane::parse("quick-tunnel"), None);
    }

    #[test]
    fn attach_acquire_cancel_and_detach_are_generation_exact() {
        let registry = AndroidForegroundRegistry::default();
        let old = registry.reserve(AndroidForegroundLane::P1Source).unwrap();
        assert!(registry.detach_if_generation(&old));
        let current = registry.reserve(AndroidForegroundLane::P1Source).unwrap();
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
        let key = registry.reserve(AndroidForegroundLane::P1Source).unwrap();
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
    fn foreground_owner_is_globally_exclusive_across_reserved_and_attached_lanes() {
        let registry = AndroidForegroundRegistry::default();
        let source = registry.reserve(AndroidForegroundLane::P1Source).unwrap();
        assert!(registry.reserve(AndroidForegroundLane::P4Source).is_err());
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
        for lane in [
            AndroidForegroundLane::P1Source,
            AndroidForegroundLane::P4Source,
        ] {
            let source = registry.reserve(lane).unwrap();
            let mut stale = source.clone();
            stale.generation += 1;
            assert!(!registry.abandon_source_exact(&stale));
            assert!(registry.abandon_source_exact(&source));
            assert!(registry.abandon_source_exact(&source));
            let target = registry.reserve(AndroidForegroundLane::P4Target).unwrap();
            assert!(!registry.abandon_source_exact(&source));
            assert!(registry.detach_if_generation(&target));
        }
    }

    #[test]
    fn retained_running_target_ignores_service_detach_until_native_release() {
        let registry = AndroidForegroundRegistry::default();
        let target = registry.reserve(AndroidForegroundLane::P4Target).unwrap();
        assert!(registry.attach_exact(&target));
        assert!(registry.retain_target_exact(&target));
        assert!(registry.cancel_exact(&target));
        assert!(!registry.detach_if_generation(&target));
        assert!(registry.reserve(AndroidForegroundLane::P1Source).is_err());
        assert!(registry.release_target_exact(&target));
        let source = registry.reserve(AndroidForegroundLane::P1Source).unwrap();
        assert!(!registry.release_target_exact(&target));
        assert!(registry.detach_if_generation(&source));
    }
}
