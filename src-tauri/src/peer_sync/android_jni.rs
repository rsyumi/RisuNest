use super::{
    android_client::{AndroidCloneStopReason, AndroidCloneStopState},
    AndroidResumableCloneJob, PeerSyncError, TransferCancellation,
};
#[cfg(target_os = "android")]
use jni::{
    objects::{JClass, JObject, JString, JValue},
    sys::{jboolean, jint, jlong, JNI_FALSE, JNI_TRUE},
    JNIEnv,
};
#[cfg(target_os = "android")]
use std::panic::AssertUnwindSafe;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

pub(crate) const RESULT_RETRYABLE_INTERRUPTION: i32 = 1;
pub(crate) const RESULT_VERIFIED_AWAITING_ACTIVATION: i32 = 2;
pub(crate) const RESULT_CANCELLED: i32 = 3;
pub(crate) const RESULT_TERMINAL_FAILURE: i32 = 4;
const NOTIFICATION_PROGRESS_STEP_BYTES: u64 = 4 * 1024 * 1024;

struct ActiveAndroidClone {
    cancellation: TransferCancellation,
    stop_reason: AndroidCloneStopState,
}

#[derive(Default)]
struct AndroidCloneRuntime {
    foreground_allowed: bool,
    jobs: HashMap<String, ActiveAndroidClone>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AndroidCloneStartDecision {
    Started,
    ForegroundUnavailable,
    AlreadyActive,
}

struct ActiveJobRegistration<'a> {
    job_id: &'a str,
    removed: bool,
}

impl ActiveJobRegistration<'_> {
    fn finish(mut self) -> AndroidCloneStopReason {
        let reason = active_runtime()
            .lock()
            .ok()
            .and_then(|mut runtime| runtime.jobs.remove(self.job_id))
            .map(|active| active.stop_reason.current())
            .unwrap_or(AndroidCloneStopReason::Running);
        self.removed = true;
        reason
    }
}

impl Drop for ActiveJobRegistration<'_> {
    fn drop(&mut self) {
        if self.removed {
            return;
        }
        if let Ok(mut runtime) = active_runtime().lock() {
            runtime.jobs.remove(self.job_id);
        }
    }
}

impl AndroidCloneRuntime {
    fn try_start(
        &mut self,
        job_id: &str,
        requires_foreground: bool,
        active: ActiveAndroidClone,
    ) -> AndroidCloneStartDecision {
        if requires_foreground && !self.foreground_allowed {
            return AndroidCloneStartDecision::ForegroundUnavailable;
        }
        if self.jobs.contains_key(job_id) {
            return AndroidCloneStartDecision::AlreadyActive;
        }
        self.jobs.insert(job_id.to_owned(), active);
        AndroidCloneStartDecision::Started
    }

    fn set_foreground_allowed(&mut self, allowed: bool) {
        self.foreground_allowed = allowed;
        if !allowed {
            for job in self.jobs.values() {
                job.stop_reason.request_pause();
                job.cancellation.cancel();
            }
        }
    }
}

fn active_runtime() -> &'static Mutex<AndroidCloneRuntime> {
    static RUNTIME: OnceLock<Mutex<AndroidCloneRuntime>> = OnceLock::new();
    RUNTIME.get_or_init(|| Mutex::new(AndroidCloneRuntime::default()))
}

fn resolve_job_root(files_root: &str, job_id: &str) -> Result<PathBuf, PeerSyncError> {
    let parsed = uuid::Uuid::parse_str(job_id)
        .map_err(|_| PeerSyncError::Storage("Android clone job identity is invalid".to_owned()))?;
    if parsed.get_version_num() != 4 || parsed.to_string() != job_id {
        return Err(PeerSyncError::Storage(
            "Android clone job identity must be a canonical UUID v4".to_owned(),
        ));
    }
    let files_root = std::fs::canonicalize(files_root)?;
    let jobs_root = std::fs::canonicalize(files_root.join("peer-clone-jobs"))?;
    if jobs_root.parent() != Some(files_root.as_path()) {
        return Err(PeerSyncError::Storage(
            "Android clone jobs root escaped private storage".to_owned(),
        ));
    }
    let job_root = jobs_root.join(job_id);
    if job_root.exists() {
        let canonical = std::fs::canonicalize(&job_root)?;
        if canonical.parent() != Some(jobs_root.as_path())
            || canonical.file_name().and_then(|value| value.to_str()) != Some(job_id)
        {
            return Err(PeerSyncError::Storage(
                "Android clone job escaped its owned root".to_owned(),
            ));
        }
        Ok(canonical)
    } else {
        Ok(job_root)
    }
}

pub(crate) fn resume_job(job_id: &str, app_data_root: &str, mut progress: impl FnMut(u64)) -> i32 {
    resume_job_with_mode(job_id, app_data_root, false, &mut progress)
}

pub(crate) fn resume_foreground_job(
    job_id: &str,
    app_data_root: &str,
    mut progress: impl FnMut(u64),
) -> i32 {
    resume_job_with_mode(job_id, app_data_root, true, &mut progress)
}

fn resume_job_with_mode(
    job_id: &str,
    app_data_root: &str,
    requires_foreground: bool,
    progress: &mut impl FnMut(u64),
) -> i32 {
    let root = match resolve_job_root(app_data_root, job_id) {
        Ok(root) => root,
        Err(_) => return RESULT_TERMINAL_FAILURE,
    };
    let cancellation = TransferCancellation::new();
    let start_decision = {
        let Ok(mut runtime) = active_runtime().lock() else {
            return RESULT_RETRYABLE_INTERRUPTION;
        };
        runtime.try_start(
            job_id,
            requires_foreground,
            ActiveAndroidClone {
                cancellation: cancellation.clone(),
                stop_reason: AndroidCloneStopState::new(),
            },
        )
    };
    match start_decision {
        AndroidCloneStartDecision::Started => {}
        AndroidCloneStartDecision::ForegroundUnavailable => {
            if let Ok(job) = AndroidResumableCloneJob::open(&root) {
                let _ = job.mark_paused();
            }
            return RESULT_RETRYABLE_INTERRUPTION;
        }
        AndroidCloneStartDecision::AlreadyActive => return RESULT_RETRYABLE_INTERRUPTION,
    }
    let registration = ActiveJobRegistration {
        job_id,
        removed: false,
    };

    let result = (|| {
        let mut job = AndroidResumableCloneJob::open(&root)?;
        if job.cancel_requested()? {
            job.discard()?;
            return Ok(RESULT_CANCELLED);
        }
        match job.download_with_progress(&cancellation, progress) {
            Ok(_) => Ok(RESULT_VERIFIED_AWAITING_ACTIVATION),
            Err(PeerSyncError::Cancelled) => Ok(RESULT_CANCELLED),
            Err(PeerSyncError::Transport(_)) => Ok(RESULT_RETRYABLE_INTERRUPTION),
            Err(error) => Err(error),
        }
    })();

    let stop_reason = registration.finish();
    match (result, stop_reason) {
        (_, AndroidCloneStopReason::Cancel) => {
            let _ = AndroidResumableCloneJob::discard_at(&root);
            RESULT_CANCELLED
        }
        (_, AndroidCloneStopReason::Pause) => {
            if let Ok(job) = AndroidResumableCloneJob::open(&root) {
                let _ = job.mark_paused();
            }
            RESULT_RETRYABLE_INTERRUPTION
        }
        (Ok(code), _) => code,
        (Err(_), _) => RESULT_TERMINAL_FAILURE,
    }
}

pub(crate) fn pause_job(job_id: &str) -> bool {
    let Ok(runtime) = active_runtime().lock() else {
        return false;
    };
    let Some(job) = runtime.jobs.get(job_id) else {
        return false;
    };
    job.stop_reason.request_pause();
    job.cancellation.cancel();
    true
}

fn set_foreground_allowed(allowed: bool) -> bool {
    let Ok(mut runtime) = active_runtime().lock() else {
        return false;
    };
    runtime.set_foreground_allowed(allowed);
    true
}

fn request_cancel_job(job_id: &str, app_data_root: &str) -> bool {
    let Ok(root) = resolve_job_root(app_data_root, job_id) else {
        return false;
    };
    if !root.exists() {
        return true;
    }
    let cancellation_requested = match AndroidResumableCloneJob::open(&root)
        .and_then(|job| job.request_cancel_for_platform_stop())
    {
        Ok(requested) => requested,
        Err(_) => return false,
    };
    if !cancellation_requested {
        return true;
    }
    let Ok(runtime) = active_runtime().lock() else {
        return false;
    };
    if let Some(job) = runtime.jobs.get(job_id) {
        job.stop_reason.request_cancel();
        job.cancellation.cancel();
    }
    true
}

pub(crate) fn cancel_and_cleanup_job(job_id: &str, app_data_root: &str) -> bool {
    if !request_cancel_job(job_id, app_data_root) {
        return false;
    }
    let Ok(root) = resolve_job_root(app_data_root, job_id) else {
        return false;
    };
    if !root.exists() {
        return true;
    }
    let cancellation_requested = AndroidResumableCloneJob::open(&root)
        .and_then(|job| job.cancel_requested())
        .unwrap_or(false);
    if !cancellation_requested {
        return true;
    }
    let Ok(runtime) = active_runtime().lock() else {
        return false;
    };
    if runtime.jobs.contains_key(job_id) {
        true
    } else {
        drop(runtime);
        AndroidResumableCloneJob::discard_at(root).is_ok()
    }
}

fn cleanup_completed_job(job_id: &str, app_data_root: &str) -> bool {
    let Ok(root) = resolve_job_root(app_data_root, job_id) else {
        return false;
    };
    if active_runtime()
        .lock()
        .map(|runtime| runtime.jobs.contains_key(job_id))
        .unwrap_or(true)
    {
        return false;
    }
    AndroidResumableCloneJob::discard_at(root).is_ok()
}

#[cfg(target_os = "android")]
fn java_string(environment: &mut JNIEnv<'_>, value: &JString<'_>) -> Option<String> {
    environment.get_string(value).ok().map(Into::into)
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_co_aiclient_risu_PeerCloneNativeBridge_resume(
    mut environment: JNIEnv,
    _class: JClass,
    job_id: JString,
    files_root: JString,
    progress_listener: JObject,
) -> jint {
    let Some(job_id) = java_string(&mut environment, &job_id) else {
        return RESULT_TERMINAL_FAILURE;
    };
    let Some(files_root) = java_string(&mut environment, &files_root) else {
        return RESULT_TERMINAL_FAILURE;
    };
    let mut last_reported = 0_u64;
    std::panic::catch_unwind(AssertUnwindSafe(|| {
        resume_job(&job_id, &files_root, |transferred_bytes| {
            if transferred_bytes <= last_reported
                || (last_reported != 0
                    && transferred_bytes - last_reported < NOTIFICATION_PROGRESS_STEP_BYTES)
            {
                return;
            }
            last_reported = transferred_bytes;
            let transferred_bytes = transferred_bytes.min(i64::MAX as u64) as jlong;
            if environment
                .call_method(
                    &progress_listener,
                    "onProgress",
                    "(J)V",
                    &[JValue::Long(transferred_bytes)],
                )
                .is_err()
            {
                let _ = environment.exception_clear();
            }
        })
    }))
    .unwrap_or(RESULT_TERMINAL_FAILURE)
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_co_aiclient_risu_PeerCloneNativeBridge_pause(
    mut environment: JNIEnv,
    _class: JClass,
    job_id: JString,
) -> jboolean {
    let Some(job_id) = java_string(&mut environment, &job_id) else {
        return JNI_FALSE;
    };
    if std::panic::catch_unwind(|| pause_job(&job_id)).unwrap_or(false) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_co_aiclient_risu_PeerCloneNativeBridge_setForegroundAllowed(
    _environment: JNIEnv,
    _class: JClass,
    allowed: jboolean,
) -> jboolean {
    if std::panic::catch_unwind(|| set_foreground_allowed(allowed == JNI_TRUE)).unwrap_or(false) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_co_aiclient_risu_PeerCloneNativeBridge_requestCancel(
    mut environment: JNIEnv,
    _class: JClass,
    job_id: JString,
    files_root: JString,
) -> jboolean {
    let Some(job_id) = java_string(&mut environment, &job_id) else {
        return JNI_FALSE;
    };
    let Some(files_root) = java_string(&mut environment, &files_root) else {
        return JNI_FALSE;
    };
    if std::panic::catch_unwind(|| request_cancel_job(&job_id, &files_root)).unwrap_or(false) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_co_aiclient_risu_PeerCloneNativeBridge_cancelAndCleanup(
    mut environment: JNIEnv,
    _class: JClass,
    job_id: JString,
    files_root: JString,
) -> jboolean {
    let Some(job_id) = java_string(&mut environment, &job_id) else {
        return JNI_FALSE;
    };
    let Some(files_root) = java_string(&mut environment, &files_root) else {
        return JNI_FALSE;
    };
    if std::panic::catch_unwind(|| cancel_and_cleanup_job(&job_id, &files_root)).unwrap_or(false) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_co_aiclient_risu_PeerCloneNativeBridge_cleanupCompleted(
    mut environment: JNIEnv,
    _class: JClass,
    job_id: JString,
    files_root: JString,
) -> jboolean {
    let Some(job_id) = java_string(&mut environment, &job_id) else {
        return JNI_FALSE;
    };
    let Some(files_root) = java_string(&mut environment, &files_root) else {
        return JNI_FALSE;
    };
    if std::panic::catch_unwind(|| cleanup_completed_job(&job_id, &files_root)).unwrap_or(false) {
        JNI_TRUE
    } else {
        JNI_FALSE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn active() -> ActiveAndroidClone {
        ActiveAndroidClone {
            cancellation: TransferCancellation::new(),
            stop_reason: AndroidCloneStopState::new(),
        }
    }

    #[test]
    fn foreground_admission_and_pause_share_one_runtime_boundary() {
        let mut runtime = AndroidCloneRuntime::default();
        assert_eq!(
            runtime.try_start("foreground", true, active()),
            AndroidCloneStartDecision::ForegroundUnavailable
        );

        runtime.set_foreground_allowed(true);
        assert_eq!(
            runtime.try_start("foreground", true, active()),
            AndroidCloneStartDecision::Started
        );
        runtime.set_foreground_allowed(false);

        let running = runtime.jobs.get("foreground").unwrap();
        assert!(running.cancellation.is_cancelled());
        assert_eq!(running.stop_reason.current(), AndroidCloneStopReason::Pause);
        assert_eq!(
            runtime.try_start("late-foreground", true, active()),
            AndroidCloneStartDecision::ForegroundUnavailable
        );
        assert_eq!(
            runtime.try_start("uidt", false, active()),
            AndroidCloneStartDecision::Started
        );
    }

    #[test]
    fn active_registration_is_removed_during_unwind() {
        let job_id = "panic-cleanup";
        {
            let mut runtime = active_runtime().lock().unwrap();
            runtime.jobs.remove(job_id);
            assert_eq!(
                runtime.try_start(job_id, false, active()),
                AndroidCloneStartDecision::Started
            );
        }

        let _ = std::panic::catch_unwind(|| {
            let _registration = ActiveJobRegistration {
                job_id,
                removed: false,
            };
            panic!("injected active clone panic");
        });

        assert!(!active_runtime().lock().unwrap().jobs.contains_key(job_id));
    }
}
