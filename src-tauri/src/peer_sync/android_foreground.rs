use crate::local_backup::CancellationProbe;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AndroidForegroundLane {
    P1Source,
}

impl AndroidForegroundLane {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "p1-source" => Some(Self::P1Source),
            _ => None,
        }
    }

    pub(crate) fn wire(self) -> &'static str {
        match self {
            Self::P1Source => "p1-source",
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
    cancellation: Arc<AtomicBool>,
    source_stop: Option<StopCallback>,
}

#[derive(Default)]
pub(crate) struct AndroidForegroundRegistry {
    next_generation: AtomicU64,
    entries: Mutex<HashMap<AndroidForegroundLane, ForegroundEntry>>,
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
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Android foreground registry is unavailable".to_owned())?;
        if entries
            .get(&lane)
            .is_some_and(|entry| entry.attached && !entry.cancellation.load(Ordering::SeqCst))
        {
            return Err("Android foreground lane is already active".to_owned());
        }
        if let Some(previous) = entries.insert(
            lane,
            ForegroundEntry {
                key: key.clone(),
                attached: false,
                cancellation: Arc::new(AtomicBool::new(false)),
                source_stop: None,
            },
        ) {
            previous.cancellation.store(true, Ordering::SeqCst);
        }
        Ok(key)
    }

    pub(crate) fn attach_exact(&self, key: &AndroidForegroundKey) -> bool {
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        let Some(entry) = entries.get_mut(&key.lane) else {
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
        let entries = self.entries.lock().ok()?;
        let entry = entries.get(&key.lane)?;
        (entry.attached && entry.key == *key && !entry.cancellation.load(Ordering::SeqCst))
            .then(|| AndroidCancellationProbe(Arc::clone(&entry.cancellation)))
    }

    pub(crate) fn set_source_stop_callback_exact(
        &self,
        key: &AndroidForegroundKey,
        callback: impl FnOnce() + Send + 'static,
    ) -> bool {
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        let Some(entry) = entries.get_mut(&key.lane) else {
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
            let Ok(mut entries) = self.entries.lock() else {
                return false;
            };
            let Some(entry) = entries.get_mut(&key.lane) else {
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

    pub(crate) fn detach_if_generation(&self, key: &AndroidForegroundKey) -> bool {
        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };
        if entries.get(&key.lane).is_none_or(|entry| entry.key != *key) {
            return false;
        }
        entries.remove(&key.lane);
        true
    }
}

pub(crate) fn registry() -> &'static AndroidForegroundRegistry {
    static REGISTRY: OnceLock<AndroidForegroundRegistry> = OnceLock::new();
    REGISTRY.get_or_init(AndroidForegroundRegistry::default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };

    #[test]
    fn only_p1_source_is_an_allowed_foreground_lane() {
        assert_eq!(
            AndroidForegroundLane::parse("p1-source"),
            Some(AndroidForegroundLane::P1Source)
        );
        assert_eq!(AndroidForegroundLane::parse("p3-target"), None);
        assert_eq!(AndroidForegroundLane::parse("quick-tunnel"), None);
    }

    #[test]
    fn attach_acquire_cancel_and_detach_are_generation_exact() {
        let registry = AndroidForegroundRegistry::default();
        let old = registry.reserve(AndroidForegroundLane::P1Source).unwrap();
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
}
