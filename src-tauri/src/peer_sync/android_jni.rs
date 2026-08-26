use super::{
    android_client::{AndroidCloneStopReason, AndroidCloneStopState},
    AndroidResumableCloneJob, PeerSyncError, TransferCancellation,
};
use jni::{
    objects::{JClass, JObject, JString, JValue},
    sys::{jboolean, jint, jlong, JNI_FALSE, JNI_TRUE},
    JNIEnv,
};
use std::{
    collections::HashMap,
    panic::AssertUnwindSafe,
    path::PathBuf,
    sync::{Mutex, OnceLock},
};

const RESULT_RETRYABLE_INTERRUPTION: jint = 1;
const RESULT_VERIFIED_AWAITING_ACTIVATION: jint = 2;
const RESULT_CANCELLED: jint = 3;
const RESULT_TERMINAL_FAILURE: jint = 4;
const NOTIFICATION_PROGRESS_STEP_BYTES: u64 = 4 * 1024 * 1024;

struct ActiveAndroidClone {
    cancellation: TransferCancellation,
    stop_reason: AndroidCloneStopState,
}

fn active_jobs() -> &'static Mutex<HashMap<String, ActiveAndroidClone>> {
    static JOBS: OnceLock<Mutex<HashMap<String, ActiveAndroidClone>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(HashMap::new()))
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

fn resume_job(job_id: &str, files_root: &str, mut progress: impl FnMut(u64)) -> jint {
    let root = match resolve_job_root(files_root, job_id) {
        Ok(root) => root,
        Err(_) => return RESULT_TERMINAL_FAILURE,
    };
    let cancellation = TransferCancellation::new();
    {
        let Ok(mut active) = active_jobs().lock() else {
            return RESULT_RETRYABLE_INTERRUPTION;
        };
        if active.contains_key(job_id) {
            return RESULT_RETRYABLE_INTERRUPTION;
        }
        active.insert(
            job_id.to_owned(),
            ActiveAndroidClone {
                cancellation: cancellation.clone(),
                stop_reason: AndroidCloneStopState::new(),
            },
        );
    }

    let result = (|| {
        let mut job = AndroidResumableCloneJob::open(&root)?;
        if job.cancel_requested()? {
            job.discard()?;
            return Ok(RESULT_CANCELLED);
        }
        match job.download_with_progress(&cancellation, &mut progress) {
            Ok(_) => Ok(RESULT_VERIFIED_AWAITING_ACTIVATION),
            Err(PeerSyncError::Cancelled) => Ok(RESULT_CANCELLED),
            Err(PeerSyncError::Transport(_)) => Ok(RESULT_RETRYABLE_INTERRUPTION),
            Err(error) => Err(error),
        }
    })();

    let stop_reason = active_jobs()
        .lock()
        .ok()
        .and_then(|mut active| active.remove(job_id))
        .map(|active| active.stop_reason.current())
        .unwrap_or(AndroidCloneStopReason::Running);
    match (result, stop_reason) {
        (_, AndroidCloneStopReason::Cancel) => {
            let _ = AndroidResumableCloneJob::discard_at(&root);
            RESULT_CANCELLED
        }
        (Ok(RESULT_CANCELLED), AndroidCloneStopReason::Pause) => RESULT_RETRYABLE_INTERRUPTION,
        (Ok(code), _) => code,
        (Err(_), _) => RESULT_TERMINAL_FAILURE,
    }
}

fn pause_job(job_id: &str) -> bool {
    let Ok(active) = active_jobs().lock() else {
        return false;
    };
    let Some(job) = active.get(job_id) else {
        return false;
    };
    job.stop_reason.request_pause();
    job.cancellation.cancel();
    true
}

fn cancel_and_cleanup_job(job_id: &str, files_root: &str) -> bool {
    let Ok(root) = resolve_job_root(files_root, job_id) else {
        return false;
    };
    if !root.exists() {
        return true;
    }
    if AndroidResumableCloneJob::request_cancel_at(&root).is_err() {
        return false;
    }
    let Ok(active) = active_jobs().lock() else {
        return false;
    };
    if let Some(job) = active.get(job_id) {
        job.stop_reason.request_cancel();
        job.cancellation.cancel();
        true
    } else {
        drop(active);
        AndroidResumableCloneJob::discard_at(root).is_ok()
    }
}

fn cleanup_completed_job(job_id: &str, files_root: &str) -> bool {
    let Ok(root) = resolve_job_root(files_root, job_id) else {
        return false;
    };
    if active_jobs()
        .lock()
        .map(|active| active.contains_key(job_id))
        .unwrap_or(true)
    {
        return false;
    }
    AndroidResumableCloneJob::discard_at(root).is_ok()
}

fn java_string(environment: &mut JNIEnv<'_>, value: &JString<'_>) -> Option<String> {
    environment.get_string(value).ok().map(Into::into)
}

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
