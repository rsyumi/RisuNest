use serde::{Deserialize, Serialize};
use std::sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use crate::persistent_store::{RevisionResult, StoreError};

const MAX_EVENT_BYTES: usize = 2048;

macro_rules! codes {
    ($name:ident { $($variant:ident),+ $(,)? }) => {
        #[derive(Clone, Copy, Debug, Deserialize, Serialize)]
        #[serde(rename_all = "kebab-case")]
        pub(crate) enum $name { $($variant),+ }
    };
}
codes!(Intent { Normal, Continue, Reroll });
codes!(DisplayMode { Off, Normal, Strong });
codes!(Profile { Normal, LowSpec });
codes!(Route { Complete, Windowed, HistoryWindow, SummaryAware, Unknown });
codes!(Fallback { None, Compatibility, MissingCapture, Unsupported, Unknown,
    SelectedConversationUnavailable, StandardHypaV3Disabled, SelectedConversationNotWindowed,
    CustomOrRemoteTokenizer, ExperimentalHypaV3, PluginEditprocess, PluginEditoutput,
    PluginChatOutput, GenerationTrigger, HistoryRegex, OldHistoryLorebookConsumer,
    AdditionalTextConsumer, InlayViewConsumer, DynamicHistoryIndirection, ExplicitHistoryReference });
codes!(Phase { SendAdmitted, HistoryMaterialization, PromptPreparation, AwaitingProvider,
    HostResponse, SemanticOutput, OutputApplication, Checkpoint, CurrentChatParser, OutputTrigger, Inlay,
    OutputListener, Tts, CompletionProcessing, FinalAcknowledgement, Settled });
codes!(Edge { Enter, Exit });
codes!(Outcome { Ok, Failed, Skipped, Stale, Aborted });
codes!(SkipReason { Ineligible, Compatibility, MissingCapture, CompleteGate, StorageFailed, OwnershipLost });
codes!(ResultKind { Buffered, Streaming, Multiline, Failed });
codes!(SavePath { Windowed, Complete, Fallback, Unknown });
codes!(Boundary { Checkpoint, Final });
codes!(Lifecycle { Active, Background, Start, Stop, Freeze, Resume, Exit, Expiry, MemoryWarning, TrimMemory });
codes!(FailureReason { ConversationChanged, TriggerStop, MemoryFailed, TokenLimit, Fenced, Error, StreamError });

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case", rename_all_fields = "camelCase", deny_unknown_fields)]
pub(crate) enum Event {
    DocumentStart {},
    Admission {
        document_sequence: u64, event_sequence: u64, generation_sequence: u64, intent: Intent,
        display_mode: DisplayMode, profile: Profile, history_limit_enabled: bool,
        history_limit_multiplier: Option<f64>, route: Route, fallback: Option<Fallback>,
        admitted_messages: Option<u32>, total_messages: Option<u32>,
    },
    Phase {
        document_sequence: u64, event_sequence: u64, generation_sequence: u64, phase: Phase, edge: Edge,
        outcome: Option<Outcome>, skip_reason: Option<SkipReason>, result_kind: Option<ResultKind>,
        save_path: Option<SavePath>, reason: Option<FailureReason>,
    },
    Persistence {
        document_sequence: u64, event_sequence: u64, generation_sequence: u64, boundary: Boundary, save_path: SavePath,
        pending_events: Option<u32>, dirty_records: Option<u32>, unacknowledged_ms: Option<u64>,
        expected_revision: Option<i64>, acknowledged_revision: Option<i64>,
    },
    Lifecycle { document_sequence: u64, event_sequence: u64, event: Lifecycle, trim_level: Option<i32> },
    ListenerPayload {
        document_sequence: u64, event_sequence: u64, generation_sequence: u64,
        listeners: u32, full_history_listeners: u32,
        conversations: Option<u32>, messages: Option<u32>, message_characters: Option<u64>,
    },
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct PhaseSlot {
    document_sequence: u64, generation_sequence: u64, phase: Option<Phase>, edge: Option<Edge>,
    phase_event_sequence: u64, path_event_sequence: u64,
    outcome: Option<Outcome>, save_path: Option<SavePath>, time_ms: u64,
    #[serde(skip)]
    listener_event_sequence: u64,
    /// Latest phase event outside the listener phase; older payloads are stale.
    #[serde(skip)]
    listener_closed_sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    listener_payload: Option<ListenerPayloadSlot>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListenerPayloadSlot {
    listeners: u32, full_history_listeners: u32,
    conversations: Option<u32>, messages: Option<u32>, message_characters: Option<u64>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
// A replaced store can reuse revisions, so this slot never assigns them to a generation.
struct CommitSlot {
    revision_correlation: &'static str,
    document_sequence: u64, expected_revision: i64, revision: Option<i64>,
    outcome: &'static str, error_code: Option<&'static str>, time_ms: u64,
}
#[derive(Default)]
struct Slots {
    phase: Option<PhaseSlot>, commit: Option<CommitSlot>,
    admission: Option<Event>, persistence: Option<Event>,
}
#[derive(Serialize)]
struct FailureEvent<'a> {
    #[serde(flatten)]
    event: &'a Event,
    admission: Option<Event>, persistence: Option<Event>, commit: Option<CommitSlot>,
}
#[derive(Default)]
struct Diagnostics {
    document: AtomicU64,
    recovery: AtomicBool,
    reload_pending: AtomicBool,
    slots: Mutex<Slots>,
}
#[cfg(not(test))]
fn with_context<T>(operation: impl FnOnce(&Diagnostics, &crate::native_log::NativeLogState) -> T) -> T {
    static STATE: std::sync::OnceLock<Diagnostics> = std::sync::OnceLock::new();
    operation(STATE.get_or_init(Diagnostics::default), &crate::native_log::global_state())
}
#[cfg(test)]
fn with_context<T>(operation: impl FnOnce(&Diagnostics, &crate::native_log::NativeLogState) -> T) -> T {
    thread_local! {
        static CONTEXT: (Diagnostics, crate::native_log::NativeLogState) =
            (Diagnostics::default(), crate::native_log::NativeLogState::for_tests());
    }
    CONTEXT.with(|(state, sink)| operation(state, sink))
}
fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|value| value.as_millis().min(u64::MAX as u128) as u64).unwrap_or_default()
}
trait DiagnosticSink {
    fn write(&self, level: &'static str, message: String);
}
impl DiagnosticSink for crate::native_log::NativeLogState {
    fn write(&self, level: &'static str, message: String) {
        self.record(level, "generation-diagnostic", message);
    }
}
struct DiagnosticWriter<F>(F);
impl<F: Fn(&'static str, String)> DiagnosticSink for DiagnosticWriter<F> {
    fn write(&self, level: &'static str, message: String) { (self.0)(level, message); }
}
fn emit(sink: &impl DiagnosticSink, level: &'static str, value: impl Serialize) {
    if let Ok(message) = serde_json::to_string(&value) {
        if message.len() <= MAX_EVENT_BYTES {
            sink.write(level, message);
        }
    }
}
fn phase_slot(slots: &mut Slots, document: u64, generation: u64, sequence: u64) -> Option<&mut PhaseSlot> {
    if let Some(previous) = slots.phase.as_ref() {
        if previous.document_sequence == document && previous.generation_sequence != generation
            && sequence <= previous.phase_event_sequence.max(previous.path_event_sequence).max(previous.listener_event_sequence) {
            return None;
        }
    }
    if slots.phase.as_ref().is_none_or(|previous|
        previous.document_sequence != document || previous.generation_sequence != generation
    ) {
        slots.phase = Some(PhaseSlot { document_sequence: document, generation_sequence: generation,
            phase: None, edge: None, outcome: None, save_path: None, time_ms: now_ms(),
            phase_event_sequence: 0, path_event_sequence: 0, listener_event_sequence: 0, listener_closed_sequence: 0, listener_payload: None });
    }
    slots.phase.as_mut()
}
impl Diagnostics {
    fn document_started(&self, sink: &impl DiagnosticSink) {
        let document = {
            let mut slots = self.slots.lock().unwrap_or_else(|error| error.into_inner());
            let document = self.document.fetch_add(1, Ordering::SeqCst).saturating_add(1);
            slots.phase = None;
            slots.admission = None;
            slots.persistence = None;
            document
        };
        let recovered = self.recovery.swap(false, Ordering::SeqCst);
        let recovered = if cfg!(target_os = "android") && !recovered { None } else { Some(recovered) };
        let os_version = match tauri_plugin_os::version() {
            tauri_plugin_os::Version::Semantic(major, minor, patch) => Some([major, minor, patch]),
            _ => None,
        };
        emit(sink, "info", serde_json::json!({"kind":"document-start", "documentSequence":document,
            "version":env!("CARGO_PKG_VERSION"), "debugBuild":cfg!(debug_assertions),
            "platform":std::env::consts::OS, "osVersion":os_version,
            "rendererRecovery":recovered}));
    }
    fn record(&self, event: Event, sink: &impl DiagnosticSink) -> Result<u64, &'static str> {
        let current = self.document.load(Ordering::SeqCst);
        let document = match &event {
            Event::DocumentStart {} => {
                self.reload_pending.store(false, Ordering::SeqCst);
                emit(sink, "info", serde_json::json!({"kind":"document-ready", "documentSequence":current, "outcome":"succeeded"}));
                return Ok(current);
            }
            Event::Admission { document_sequence, history_limit_multiplier, .. } => {
                if history_limit_multiplier.is_some_and(|value| !value.is_finite() || value <= 0.0) {
                    return Err("invalid-diagnostic-number");
                }
                *document_sequence
            }
            Event::Phase { document_sequence, .. } | Event::Persistence { document_sequence, .. }
            | Event::Lifecycle { document_sequence, .. } | Event::ListenerPayload { document_sequence, .. } => *document_sequence,
        };
        if current == 0 || document != current { return Err("stale-diagnostic-document"); }
        let event_sequence = match &event {
            Event::Admission { event_sequence, .. } | Event::Phase { event_sequence, .. }
            | Event::Persistence { event_sequence, .. } | Event::Lifecycle { event_sequence, .. }
            | Event::ListenerPayload { event_sequence, .. } => *event_sequence,
            Event::DocumentStart {} => unreachable!(),
        };
        if event_sequence == 0 { return Err("invalid-diagnostic-number"); }
        match &event {
            Event::Admission { event_sequence, .. } => {
                let mut slots = self.slots.lock().unwrap_or_else(|error| error.into_inner());
                if document != self.document.load(Ordering::SeqCst) { return Err("stale-diagnostic-document"); }
                if slots.admission.as_ref().is_none_or(|previous|
                    matches!(previous, Event::Admission { event_sequence: sequence, .. } if sequence < event_sequence)
                ) {
                    slots.admission = Some(event.clone());
                }
            }
            Event::Phase { document_sequence, event_sequence, generation_sequence, phase, edge, outcome, save_path, .. } => {
                let mut slots = self.slots.lock().unwrap_or_else(|error| error.into_inner());
                if document != self.document.load(Ordering::SeqCst) { return Err("stale-diagnostic-document"); }
                if let Some(slot) = phase_slot(&mut slots, *document_sequence, *generation_sequence, *event_sequence) {
                    if *event_sequence > slot.phase_event_sequence {
                        slot.phase = Some(*phase);
                        slot.edge = Some(*edge);
                        slot.outcome = *outcome;
                        slot.phase_event_sequence = *event_sequence;
                        slot.time_ms = now_ms();
                    }
                    // The listener payload describes only the listener phase.
                    if !matches!(phase, Phase::OutputListener) && *event_sequence > slot.listener_closed_sequence {
                        slot.listener_closed_sequence = *event_sequence;
                        if *event_sequence > slot.listener_event_sequence { slot.listener_payload = None; }
                    }
                    if save_path.is_some() && *event_sequence > slot.path_event_sequence {
                        slot.save_path = *save_path;
                        slot.path_event_sequence = *event_sequence;
                    }
                }
            }
            Event::Persistence { document_sequence, event_sequence, generation_sequence, save_path, .. } => {
                let mut slots = self.slots.lock().unwrap_or_else(|error| error.into_inner());
                if document != self.document.load(Ordering::SeqCst) { return Err("stale-diagnostic-document"); }
                if let Some(slot) = phase_slot(&mut slots, *document_sequence, *generation_sequence, *event_sequence) {
                    if *event_sequence > slot.path_event_sequence {
                        slot.save_path = Some(*save_path);
                        slot.path_event_sequence = *event_sequence;
                    }
                }
                if slots.persistence.as_ref().is_none_or(|previous|
                    matches!(previous, Event::Persistence { event_sequence: sequence, .. } if sequence < event_sequence)
                ) {
                    slots.persistence = Some(event.clone());
                }
            }
            Event::ListenerPayload {
                document_sequence, event_sequence, generation_sequence, listeners, full_history_listeners,
                conversations, messages, message_characters,
            } => {
                let mut slots = self.slots.lock().unwrap_or_else(|error| error.into_inner());
                if document != self.document.load(Ordering::SeqCst) { return Err("stale-diagnostic-document"); }
                if let Some(slot) = phase_slot(&mut slots, *document_sequence, *generation_sequence, *event_sequence) {
                    if *event_sequence > slot.listener_event_sequence && *event_sequence > slot.listener_closed_sequence {
                        slot.listener_payload = Some(ListenerPayloadSlot {
                            listeners: *listeners, full_history_listeners: *full_history_listeners,
                            conversations: *conversations, messages: *messages, message_characters: *message_characters,
                        });
                        slot.listener_event_sequence = *event_sequence;
                    }
                }
            }
            _ => {}
        }
        // Successful high-frequency events only update the crash context. Keep
        // failures and lifecycle boundaries in the user-visible log.
        match &event {
            Event::Phase { outcome: Some(Outcome::Failed), generation_sequence, .. } => {
                let context = {
                    let slots = self.slots.lock().unwrap_or_else(|error| error.into_inner());
                    FailureEvent { event: &event,
                        admission: slots.admission.as_ref().filter(|value| matches!(value,
                            Event::Admission { document_sequence: owner, generation_sequence: generation, .. }
                                if *owner == document && generation == generation_sequence)).cloned(),
                        persistence: slots.persistence.as_ref().filter(|value| matches!(value,
                            Event::Persistence { document_sequence: owner, generation_sequence: generation, .. }
                                if *owner == document && generation == generation_sequence)).cloned(),
                        commit: slots.commit.as_ref().filter(|slot| slot.document_sequence == document).cloned() }
                };
                emit(sink, "error", context);
            }
            Event::Lifecycle { .. } => emit(sink, "info", event),
            _ => {}
        }
        Ok(current)
    }
    fn commit(&self, document: u64, expected: i64, result: &Result<RevisionResult, StoreError>, committed_revision: Option<i64>, sink: &impl DiagnosticSink) {
        let (revision, outcome, error_code) = match result {
            Ok(value) => (Some(value.revision), "ok", None),
            Err(error) => {
                let code = match error {
                    StoreError::RevisionConflict { .. } => "revision-conflict",
                    StoreError::SnapshotReleased => "snapshot-released", StoreError::RawBodyUnavailable => "raw-body-unavailable",
                    StoreError::CommitBusy => "commit-busy", StoreError::CommitDecode { .. } => "commit-decode",
                    StoreError::Committed { .. } => "committed", StoreError::Validation { .. } => "validation",
                    StoreError::SchemaMismatch { .. } => "schema-mismatch", StoreError::Store { .. } => "store-error",
                };
                let revision = if let StoreError::Committed { revision, .. } = error { Some(*revision) } else { committed_revision };
                (revision, if revision.is_some() { "committed-error" } else { "failed" }, Some(code))
            }
        };
        let slot = CommitSlot { revision_correlation: "store-unknown", document_sequence: document, expected_revision: expected, revision, outcome, error_code, time_ms: now_ms() };
        self.slots.lock().unwrap_or_else(|error| error.into_inner()).commit = Some(slot.clone());
        if result.is_err() { emit(sink, "error", serde_json::json!({"kind":"native-commit", "commit":slot})); }
    }
    fn reload_result(&self, outcome: &'static str, sink: &impl DiagnosticSink) {
        match outcome {
            "requested" => self.reload_pending.store(true, Ordering::SeqCst),
            "failed" if self.reload_pending.swap(false, Ordering::SeqCst) => {},
            _ => return,
        }
        emit(sink, "info", serde_json::json!({"kind":"renderer-reload",
            "documentSequence":self.document.load(Ordering::SeqCst), "outcome":outcome}));
    }
    #[cfg(any(target_os = "android", test))]
    fn service_refused(&self, foreground: bool, background: bool, sink: &impl DiagnosticSink) {
        emit(sink, "error", serde_json::json!({"kind":"background-service",
            "documentSequence":self.document.load(Ordering::SeqCst),
            "stage":if foreground { "foreground" } else { "start" },
            "outcome":if background { "background-refused" } else { "failed" }}));
    }
    fn renderer_exit(&self, reason: RendererExit, sink: &impl DiagnosticSink) {
        self.recovery.store(true, Ordering::SeqCst);
        let now = now_ms();
        let (current, phase, commit, admission, persistence) = {
            let slots = self.slots.lock().unwrap_or_else(|error| error.into_inner());
            let current = self.document.load(Ordering::SeqCst);
            (current,
                slots.phase.as_ref().filter(|slot| current != 0 && slot.document_sequence == current).cloned(),
                slots.commit.as_ref().filter(|slot| current != 0 && slot.document_sequence == current).cloned(),
                slots.admission.clone(), slots.persistence.clone())
        };
        emit(sink, "error", serde_json::json!({"kind":"renderer-exit", "documentSequence":current,
            "reason":reason, "phaseAgeMs":phase.as_ref().map(|value| now.saturating_sub(value.time_ms)),
            "commitAgeMs":commit.as_ref().map(|value| now.saturating_sub(value.time_ms)),
            "phase":phase, "commit":commit, "admission":admission, "persistence":persistence}));
    }
}

#[derive(Serialize)]
#[serde(tag = "platform", rename_all = "kebab-case", rename_all_fields = "camelCase")]
pub(crate) enum RendererExit {
    #[cfg(any(target_os = "macos", target_os = "ios", test))]
    #[cfg_attr(target_os = "ios", serde(rename = "ios"))]
    #[cfg_attr(not(target_os = "ios"), serde(rename = "macos"))]
    Apple { reason: &'static str },
    #[cfg(any(windows, test))]
    Windows { kind: i32, reason: Option<i32>, exit_code: Option<i32> },
    #[cfg(any(target_os = "linux", test))]
    Linux { reason: &'static str },
    #[cfg(any(target_os = "android", test))]
    Android { did_crash: bool },
}
#[cfg(any(target_os = "macos", target_os = "ios", test))]
impl RendererExit {
    /// Names WebKit's private `_WKProcessTerminationReason` value.
    pub(crate) fn apple(reason: Option<isize>) -> Self {
        RendererExit::Apple { reason: match reason {
            Some(0) => "memory-limit",
            Some(1) => "cpu-limit",
            Some(2) => "client-request",
            Some(3) => "crash",
            Some(4) => "shared-process-crash-limit",
            _ => "unknown",
        } }
    }
}
pub(crate) fn document_started() { with_context(|state, sink| state.document_started(sink)); }
pub(crate) fn commit_document() -> u64 { with_context(|state, _| state.document.load(Ordering::SeqCst)) }
pub(crate) fn commit_finished(document: u64, expected: i64, result: &Result<RevisionResult, StoreError>, committed_revision: Option<i64>) {
    with_context(|state, sink| state.commit(document, expected, result, committed_revision, sink));
}
pub(crate) fn renderer_exit(reason: RendererExit) { with_context(|state, sink| state.renderer_exit(reason, sink)); }
pub(crate) fn reload_result(outcome: &'static str) { with_context(|state, sink| state.reload_result(outcome, sink)); }
#[cfg(target_os = "android")]
fn service_refused(foreground: bool, background: bool) { with_context(|state, sink| state.service_refused(foreground, background, sink)); }

#[tauri::command]
pub(crate) fn native_generation_diagnostic(window: tauri::WebviewWindow, event: Event) -> Result<u64, &'static str> {
    // Existing IPC origin and main-frame invoke-key checks isolate plugin frames from native commands.
    if window.label() != "main" { return Err("diagnostic-main-document-required"); }
    with_context(|state, sink| state.record(event, &DiagnosticWriter(|level, message| {
        let sink = sink.clone();
        tauri::async_runtime::spawn_blocking(move || sink.record(level, "generation-diagnostic", message));
    })))
}

#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_github_rsyumi_risunest_MainActivity_recordRendererExit(
    _env: jni::JNIEnv, _activity: jni::objects::JObject, did_crash: jni::sys::jboolean,
) {
    let _ = std::panic::catch_unwind(|| {
        renderer_exit(RendererExit::Android { did_crash: did_crash != 0 });
        reload_result("requested");
    });
}

/// Android refused to start the background service, or to bring it to the foreground.
#[cfg(target_os = "android")]
#[no_mangle]
pub extern "system" fn Java_io_github_rsyumi_risunest_BackgroundServiceLog_recordRefused(
    _env: jni::JNIEnv, _class: jni::objects::JClass, foreground: jni::sys::jboolean, background: jni::sys::jboolean,
) {
    let _ = std::panic::catch_unwind(|| service_refused(foreground != 0, background != 0));
}

#[cfg(test)]
mod tests {
    use super::*;
    fn phase(document: u64) -> Event {
        serde_json::from_value(serde_json::json!({"kind":"phase", "documentSequence":document, "eventSequence":1,
            "generationSequence":1, "phase":"final-acknowledgement", "edge":"enter", "savePath":"windowed"})).unwrap()
    }
    fn tail(sink: &crate::native_log::NativeLogState) -> serde_json::Value {
        serde_json::from_str(&sink.tail(Some(1))[0].message).unwrap()
    }
    #[test]
    fn renderer_tail_distinguishes_native_completion_from_frontend_acknowledgement() {
        let state = Diagnostics::default();
        let sink = crate::native_log::NativeLogState::for_tests();
        state.document_started(&sink);
        state.record(phase(1), &sink).unwrap();
        let before = sink.tail(None).len();
        state.commit(1, 7, &Ok(RevisionResult { revision: 8 }), None, &sink);
        assert_eq!(sink.tail(None).len(), before);
        state.record(serde_json::from_value(serde_json::json!({"kind":"persistence", "documentSequence":1, "eventSequence":2,
            "generationSequence":1, "boundary":"final", "savePath":"complete"})).unwrap(), &sink).unwrap();
        state.record(serde_json::from_value(serde_json::json!({"kind":"phase", "documentSequence":1, "eventSequence":3,
            "generationSequence":1, "phase":"final-acknowledgement", "edge":"enter"})).unwrap(), &sink).unwrap();
        state.renderer_exit(RendererExit::apple(Some(0)), &sink);
        let value = tail(&sink);
        assert_eq!(value["reason"]["reason"], "memory-limit");
        assert_eq!(value["phase"]["savePath"], "complete");
        assert_eq!(value["phase"]["edge"], "enter");
        assert_eq!(value["commit"]["revision"], 8);
        assert_eq!(value["commit"]["revisionCorrelation"], "store-unknown");
        assert!(value["phaseAgeMs"].is_number());
        let late_failure = serde_json::from_value(serde_json::json!({"kind":"phase", "documentSequence":1,
            "eventSequence":1, "generationSequence":1, "phase":"awaiting-provider", "edge":"exit", "outcome":"failed"})).unwrap();
        state.record(late_failure, &sink).unwrap();
        assert_eq!(tail(&sink)["eventSequence"], 1);
        assert_eq!(tail(&sink)["outcome"], "failed");
        let old_path = serde_json::from_value(serde_json::json!({"kind":"persistence", "documentSequence":1,
            "eventSequence":1, "generationSequence":1, "boundary":"checkpoint", "savePath":"fallback"})).unwrap();
        let before = sink.tail(None).len();
        state.record(old_path, &sink).unwrap();
        assert_eq!(sink.tail(None).len(), before);
        state.renderer_exit(RendererExit::apple(None), &sink);
        assert_eq!(tail(&sink)["reason"]["reason"], "unknown");
        assert_eq!(tail(&sink)["phase"]["phase"], "final-acknowledgement");
        assert_eq!(tail(&sink)["phase"]["phaseEventSequence"], 3);
        assert_eq!(tail(&sink)["phase"]["savePath"], "complete");
        state.record(serde_json::from_value(serde_json::json!({"kind":"phase", "documentSequence":1, "eventSequence":4,
            "generationSequence":2, "phase":"send-admitted", "edge":"enter"})).unwrap(), &sink).unwrap();
        state.renderer_exit(RendererExit::Apple { reason: "unknown" }, &sink);
        assert!(tail(&sink)["phase"]["savePath"].is_null());
        let before = sink.tail(None).len();
        state.record(phase(1), &sink).unwrap();
        assert_eq!(sink.tail(None).len(), before);
        state.renderer_exit(RendererExit::Apple { reason: "unknown" }, &sink);
        assert_eq!(tail(&sink)["phase"]["generationSequence"], 2);
        assert_eq!(tail(&sink)["phase"]["phase"], "send-admitted");
        // A persistence IPC can finish first without suppressing its preceding phase IPC.
        state.record(serde_json::from_value(serde_json::json!({"kind":"persistence", "documentSequence":1,
            "eventSequence":6, "generationSequence":3, "boundary":"final", "savePath":"complete"})).unwrap(), &sink).unwrap();
        state.record(serde_json::from_value(serde_json::json!({"kind":"phase", "documentSequence":1,
            "eventSequence":5, "generationSequence":3, "phase":"final-acknowledgement", "edge":"enter",
            "savePath":"windowed"})).unwrap(), &sink).unwrap();
        state.renderer_exit(RendererExit::Apple { reason: "unknown" }, &sink);
        assert_eq!(tail(&sink)["phase"]["phase"], "final-acknowledgement");
        assert_eq!(tail(&sink)["phase"]["savePath"], "complete");
        state.document_started(&sink);
        state.commit(1, 8, &Ok(RevisionResult { revision: 9 }), None, &sink);
        assert!(state.record(phase(1), &sink).is_err());
        state.renderer_exit(RendererExit::Android { did_crash: false }, &sink);
        let value = tail(&sink);
        assert!(value["phase"].is_null());
        assert!(value["commit"].is_null());
    }
    #[test]
    fn real_store_commits_publish_outcome_even_before_frontend_acknowledgement() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = crate::persistent_store::PersistentStore::open(directory.path()).unwrap();
        let staging = store.replace_begin().unwrap();
        store.replace_put_root(&staging.staging_id, &serde_json::json!({"username":"synthetic"})).unwrap();
        store.replace_commit(&staging.staging_id, Some(0)).unwrap();
        document_started();
        let document = commit_document();
        let input = crate::persistent_store::WorkingSetCommit { expected_revision:1,
            root:Some(serde_json::json!({"username":"synthetic-next"})), ..Default::default() };
        let revision = store.commit(&input).unwrap().revision;
        renderer_exit(RendererExit::Apple { reason:"unknown" });
        let value = with_context(|_, sink| tail(sink));
        assert_eq!(value["commit"]["revision"], revision);
        assert_eq!(value["commit"]["documentSequence"], document);
        assert!(matches!(store.commit(&input), Err(StoreError::RevisionConflict { .. })));
        let value = with_context(|_, sink| tail(sink));
        assert_eq!(value["commit"]["errorCode"], "revision-conflict");
        assert_eq!(store.revision().unwrap(), revision);
        store.device_store().unwrap().connection().execute_batch(
            "CREATE TRIGGER diagnostic_cleanup_failure BEFORE DELETE ON lww_intents
             BEGIN SELECT RAISE(FAIL, 'synthetic-cleanup-failure'); END;"
        ).unwrap();
        let input = crate::persistent_store::WorkingSetCommit { expected_revision: revision,
            root: Some(serde_json::json!({"username":"synthetic-durable"})), ..Default::default() };
        assert!(matches!(store.commit(&input), Err(StoreError::Store { .. })));
        let value = with_context(|_, sink| tail(sink));
        assert_eq!(value["commit"]["outcome"], "committed-error");
        assert_eq!(value["commit"]["revision"], revision + 1);
        assert_eq!(value["commit"]["errorCode"], "store-error");
        store.device_store().unwrap().connection().execute_batch("DROP TRIGGER diagnostic_cleanup_failure;").unwrap();
        drop(store);
        let store = crate::persistent_store::PersistentStore::open(directory.path()).unwrap();
        assert_eq!(store.read_root(None).unwrap().value["username"], "synthetic-durable");
    }
    #[test]
    fn alias_and_gallery_commits_publish_the_actual_revision() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = crate::persistent_store::PersistentStore::open(directory.path()).unwrap();
        document_started();
        let alias = crate::persistent_store::AssetAlias {
            key: "synthetic-inlay".into(), object_hash: None, kind: "inlay".into(), size: 0,
            mime: "image/png".into(), name: "synthetic".into(), ext: "png".into(),
            inlay_type: Some("image".into()), width: None, height: None,
            metadata: serde_json::json!({}),
        };
        let mut revision = store.revision().unwrap();
        for operation in 0..4 {
            let expected = revision;
            revision = match operation {
                0 | 2 => store.commit_asset_alias(&alias, expected).unwrap().revision,
                1 => store.delete_asset_alias("inlay", &alias.key, expected).unwrap().revision,
                _ => store.gallery_delete(&[alias.key.clone()], expected, None).unwrap().revision,
            };
            renderer_exit(RendererExit::Linux { reason:"crashed" });
            let value = with_context(|_, sink| tail(sink));
            assert_eq!(value["commit"]["expectedRevision"], expected);
            assert_eq!(value["commit"]["revision"], revision);
            assert_eq!(value["commit"]["outcome"], "ok");
            assert_eq!(store.revision().unwrap(), revision);
        }
        assert!(matches!(store.delete_asset_alias("inlay", &alias.key, revision - 1),
            Err(StoreError::RevisionConflict { .. })));
        assert_eq!(with_context(|_, sink| tail(sink))["commit"]["errorCode"], "revision-conflict");
    }
    #[test]
    fn frontend_readiness_never_changes_the_native_document_sequence() {
        let state = Diagnostics::default();
        let sink = crate::native_log::NativeLogState::for_tests();
        assert_eq!(state.record(Event::DocumentStart {}, &sink).unwrap(), 0);
        state.document_started(&sink);
        // The first document never became ready. The next frontend can arrive
        // before its native page callback without claiming that callback's epoch.
        assert_eq!(state.record(Event::DocumentStart {}, &sink).unwrap(), 1);
        state.document_started(&sink);
        state.record(phase(2), &sink).unwrap();
        assert_eq!(state.record(Event::DocumentStart {}, &sink).unwrap(), 2);
        assert_eq!(state.record(Event::DocumentStart {}, &sink).unwrap(), 2);
        state.renderer_exit(RendererExit::Android { did_crash:true }, &sink);
        assert_eq!(tail(&sink)["documentSequence"], 2);
        assert_eq!(tail(&sink)["phase"]["documentSequence"], 2);
        assert!(state.record(phase(1), &sink).is_err());
    }
    #[test]
    fn delayed_log_writes_do_not_reassign_a_new_documents_phase() {
        let state = Diagnostics::default();
        let sink = crate::native_log::NativeLogState::for_tests();
        let pending = std::cell::RefCell::new(Vec::new());
        let deferred = DiagnosticWriter(|level, message| pending.borrow_mut().push((level, message)));
        state.document_started(&deferred);
        assert_eq!(state.record(Event::DocumentStart {}, &deferred).unwrap(), 1);
        state.record(phase(1), &deferred).unwrap();
        state.document_started(&deferred);
        assert_eq!(state.record(Event::DocumentStart {}, &deferred).unwrap(), 2);
        state.record(phase(2), &deferred).unwrap();
        assert!(sink.tail(None).is_empty());
        for (level, message) in pending.into_inner().into_iter().rev() {
            sink.write(level, message);
        }
        state.renderer_exit(RendererExit::Android { did_crash:true }, &sink);
        assert_eq!(tail(&sink)["documentSequence"], 2);
        assert_eq!(tail(&sink)["phase"]["documentSequence"], 2);
        assert_eq!(tail(&sink)["phase"]["phase"], "final-acknowledgement");
        assert!(state.record(phase(2), &sink).is_ok());
        assert!(state.record(phase(1), &sink).is_err());
    }
    #[test]
    fn routine_diagnostics_preserve_errors_and_crash_context_without_filling_the_log() {
        let directory = tempfile::tempdir().unwrap();
        let sink = crate::native_log::NativeLogState::initialize(directory.path());
        std::fs::write(sink.file_path(), vec![b'x'; crate::native_log::FILE_ROTATE_BYTES + 1]).unwrap();
        let state = Diagnostics::default();
        state.document_started(&sink);
        assert!(sink.file_path().with_extension("log.1").exists());
        sink.record("error", "synthetic", "earlier failure");
        let before = sink.tail(None).len();
        for sequence in 1..=crate::native_log::RING_CAPACITY + 2 {
            let mut event = phase(1);
            if let Event::Phase { event_sequence, .. } = &mut event { *event_sequence = sequence as u64; }
            state.record(event, &sink).unwrap();
        }
        assert_eq!(sink.tail(None).len(), before);
        assert_eq!(sink.tail(Some(1))[0].message, "earlier failure");
        state.record(serde_json::from_value(serde_json::json!({
            "kind":"admission", "documentSequence":1, "eventSequence":2000, "generationSequence":1,
            "intent":"normal", "displayMode":"normal", "profile":"normal",
            "historyLimitEnabled":true, "route":"history-window", "admittedMessages":12
        })).unwrap(), &sink).unwrap();
        state.renderer_exit(RendererExit::Linux { reason:"crashed" }, &sink);
        assert_eq!(tail(&sink)["phase"]["phaseEventSequence"], crate::native_log::RING_CAPACITY + 2);
        assert_eq!(tail(&sink)["admission"]["admittedMessages"], 12);
        let mut failure = phase(1);
        if let Event::Phase { outcome, event_sequence, reason, .. } = &mut failure {
            *outcome = Some(Outcome::Failed);
            *event_sequence = 2001;
            *reason = Some(FailureReason::StreamError);
        }
        state.record(failure, &sink).unwrap();
        assert_eq!(tail(&sink)["outcome"], "failed");
        assert_eq!(tail(&sink)["reason"], "stream-error");
        assert_eq!(tail(&sink)["admission"]["route"], "history-window");
        assert!(std::fs::read_to_string(sink.file_path()).unwrap().contains("generation-diagnostic"));
    }
    #[test]
    fn diagnostic_inputs_reject_text_and_unknown_codes() {
        let state = Diagnostics::default();
        let sink = crate::native_log::NativeLogState::for_tests();
        state.document_started(&sink);
        for event in ["stop", "exit", "active", "start", "freeze", "resume"] {
            state.record(serde_json::from_value(serde_json::json!({"kind":"lifecycle", "documentSequence":1,
                "eventSequence":1, "event":event})).unwrap(), &sink).unwrap();
            assert_eq!(tail(&sink)["event"], event);
        }
        state.record(serde_json::from_value(serde_json::json!({"kind":"lifecycle", "documentSequence":1,
            "eventSequence":1, "event":"trim-memory", "trimLevel":20})).unwrap(), &sink).unwrap();
        assert_eq!(tail(&sink)["trimLevel"], 20);
        state.service_refused(false, true, &sink);
        assert_eq!(tail(&sink), serde_json::json!({"kind":"background-service", "documentSequence":1,
            "stage":"start", "outcome":"background-refused"}));
        state.service_refused(true, false, &sink);
        assert_eq!(tail(&sink)["stage"], "foreground");
        assert_eq!(tail(&sink)["outcome"], "failed");
        state.record(serde_json::from_value(serde_json::json!({"kind":"phase", "documentSequence":1,
            "eventSequence":2, "generationSequence":1, "phase":"completion-processing", "edge":"enter"})).unwrap(), &sink).unwrap();
        state.renderer_exit(RendererExit::Linux { reason:"crashed" }, &sink);
        assert_eq!(tail(&sink)["phase"]["phase"], "completion-processing");
        let entry = sink.tail(Some(1)).pop().unwrap();
        assert_eq!(entry.target, "generation-diagnostic");
        assert!(serde_json::from_str::<serde_json::Value>(&entry.message).is_ok());
        for value in [
            serde_json::json!({"kind":"document-start", "text":"private"}),
            serde_json::json!({"kind":"phase", "documentSequence":1, "eventSequence":1,"generationSequence":1,"phase":"private", "edge":"enter"}),
            serde_json::json!({"kind":"lifecycle", "documentSequence":1, "eventSequence":1,"event":"background", "message":"private"}),
            serde_json::json!({"kind":"phase", "documentSequence":1, "eventSequence":1,"generationSequence":1,"phase":"prompt-preparation",
                "edge":"exit", "outcome":"failed", "reason":"private"}),
        ] { assert!(serde_json::from_value::<Event>(value).is_err()); }
    }
    #[test]
    fn listener_payload_reaches_a_renderer_exit_inside_the_listener_phase_only() {
        let state = Diagnostics::default();
        let sink = crate::native_log::NativeLogState::for_tests();
        state.document_started(&sink);
        let before = sink.tail(None).len();
        let record = |value: serde_json::Value| state.record(serde_json::from_value(value).unwrap(), &sink).unwrap();
        record(serde_json::json!({"kind":"phase", "documentSequence":1, "eventSequence":1, "generationSequence":1,
            "phase":"output-listener", "edge":"enter"}));
        record(serde_json::json!({"kind":"listener-payload", "documentSequence":1, "eventSequence":2,
            "generationSequence":1, "listeners":2, "fullHistoryListeners":1}));
        record(serde_json::json!({"kind":"listener-payload", "documentSequence":1, "eventSequence":3,
            "generationSequence":1, "listeners":2, "fullHistoryListeners":1, "conversations":40,
            "messages":120000, "messageCharacters":90000000u64}));
        assert_eq!(sink.tail(None).len(), before);
        state.renderer_exit(RendererExit::Linux { reason:"crashed" }, &sink);
        let exit = tail(&sink);
        assert_eq!(exit["phase"]["phase"], "output-listener");
        assert_eq!(exit["phase"]["listenerPayload"], serde_json::json!({"listeners":2, "fullHistoryListeners":1,
            "conversations":40, "messages":120000, "messageCharacters":90000000u64}));
        record(serde_json::json!({"kind":"phase", "documentSequence":1, "eventSequence":4, "generationSequence":1,
            "phase":"tts", "edge":"enter"}));
        state.renderer_exit(RendererExit::Linux { reason:"crashed" }, &sink);
        assert!(tail(&sink)["phase"].get("listenerPayload").is_none());
        // A payload sent before the next phase can arrive after it.
        record(serde_json::json!({"kind":"phase", "documentSequence":1, "eventSequence":5, "generationSequence":2,
            "phase":"output-listener", "edge":"enter"}));
        record(serde_json::json!({"kind":"listener-payload", "documentSequence":1, "eventSequence":6,
            "generationSequence":2, "listeners":1, "fullHistoryListeners":1}));
        record(serde_json::json!({"kind":"phase", "documentSequence":1, "eventSequence":8, "generationSequence":2,
            "phase":"tts", "edge":"enter"}));
        record(serde_json::json!({"kind":"listener-payload", "documentSequence":1, "eventSequence":7,
            "generationSequence":2, "listeners":1, "fullHistoryListeners":1, "conversations":40}));
        state.renderer_exit(RendererExit::Linux { reason:"crashed" }, &sink);
        assert_eq!(tail(&sink)["phase"]["phase"], "tts");
        assert!(tail(&sink)["phase"].get("listenerPayload").is_none());
        assert!(serde_json::from_value::<Event>(serde_json::json!({"kind":"listener-payload", "documentSequence":1,
            "eventSequence":5, "generationSequence":1, "listeners":1, "fullHistoryListeners":1, "name":"private"})).is_err());
    }
    #[test]
    fn failures_are_safe_and_logging_failure_or_disabled_files_does_not_change_slots() {
        let directory = tempfile::tempdir().unwrap();
        let sink = crate::native_log::NativeLogState::initialize(directory.path());
        sink.set_file_enabled(false).unwrap();
        let state = Diagnostics::default();
        state.document_started(&sink);
        state.commit(1, 2, &Err(StoreError::Store { message:"synthetic-private-canary".into() }), None, &sink);
        assert!(!sink.file_path().exists());
        assert_eq!(tail(&sink)["commit"]["errorCode"], "store-error");
        sink.set_file_enabled(true).unwrap();
        std::fs::create_dir(sink.file_path()).unwrap();
        state.record(phase(1), &sink).unwrap();
        state.renderer_exit(RendererExit::Windows { kind:1, reason:Some(2), exit_code:Some(3) }, &sink);
        for entry in sink.tail(None) {
            assert!(!entry.message.contains("synthetic-private-canary"));
            assert!(entry.message.len() <= MAX_EVENT_BYTES);
        }
        assert_eq!(tail(&sink)["phase"]["generationSequence"], 1);
    }
    #[test]
    fn early_renderer_callback_and_contended_slots_preserve_evidence() {
        let state = Diagnostics::default();
        let sink = crate::native_log::NativeLogState::for_tests();
        let before = sink.tail(None).len();
        state.reload_result("failed", &sink);
        assert_eq!(sink.tail(None).len(), before);
        state.reload_result("requested", &sink);
        state.reload_result("failed", &sink);
        assert_eq!(tail(&sink)["outcome"], "failed");
        state.reload_result("requested", &sink);
        state.record(Event::DocumentStart {}, &sink).unwrap();
        assert_eq!(tail(&sink)["kind"], "document-ready");
        state.reload_result("failed", &sink);
        assert_eq!(tail(&sink)["kind"], "document-ready");
        state.renderer_exit(RendererExit::Android { did_crash:true }, &sink);
        assert!(tail(&sink)["phase"].is_null());
        fn while_contended(state: &Diagnostics, operation: impl FnOnce() + Send) {
            std::thread::scope(|scope| {
                let held = state.slots.lock().unwrap_or_else(|error| error.into_inner());
                let (started_tx, started_rx) = std::sync::mpsc::channel();
                let (finished_tx, finished_rx) = std::sync::mpsc::channel();
                let worker = scope.spawn(move || {
                    started_tx.send(()).unwrap();
                    operation();
                    finished_tx.send(()).unwrap();
                });
                started_rx.recv().unwrap();
                let blocked = finished_rx.recv_timeout(std::time::Duration::from_millis(100));
                drop(held);
                worker.join().unwrap();
                assert!(matches!(blocked, Err(std::sync::mpsc::RecvTimeoutError::Timeout)));
            });
        }
        state.document_started(&sink);
        while_contended(&state, || state.commit(1, 0, &Ok(RevisionResult { revision:1 }), None, &sink));
        state.renderer_exit(RendererExit::Linux { reason:"crashed" }, &sink);
        assert_eq!(tail(&sink)["commit"]["revision"], 1);
        while_contended(&state, || { state.record(phase(1), &sink).unwrap(); });
        while_contended(&state, || {
            state.record(serde_json::from_value(serde_json::json!({"kind":"persistence", "documentSequence":1,
                "eventSequence":2, "generationSequence":1, "boundary":"final", "savePath":"complete"})).unwrap(), &sink).unwrap();
        });
        while_contended(&state, || state.renderer_exit(RendererExit::Linux { reason:"crashed" }, &sink));
        assert_eq!(tail(&sink)["phase"]["phase"], "final-acknowledgement");
        assert_eq!(tail(&sink)["phase"]["savePath"], "complete");
        assert_eq!(tail(&sink)["commit"]["revision"], 1);
        while_contended(&state, || state.document_started(&sink));
        state.renderer_exit(RendererExit::Linux { reason:"crashed" }, &sink);
        assert_eq!(tail(&sink)["documentSequence"], 2);
        assert!(tail(&sink)["phase"].is_null());
        assert!(tail(&sink)["commit"].is_null());
        std::thread::scope(|scope| {
            assert!(scope.spawn(|| {
                let _held = state.slots.lock().unwrap();
                panic!("synthetic diagnostic slot poison");
            }).join().is_err());
        });
        state.record(phase(2), &sink).unwrap();
        state.commit(2, 1, &Ok(RevisionResult { revision:2 }), None, &sink);
        state.renderer_exit(RendererExit::Linux { reason:"crashed" }, &sink);
        assert_eq!(tail(&sink)["phase"]["documentSequence"], 2);
        assert_eq!(tail(&sink)["commit"]["revision"], 2);
        state.document_started(&sink);
        state.renderer_exit(RendererExit::Linux { reason:"crashed" }, &sink);
        assert_eq!(tail(&sink)["documentSequence"], 3);
        assert!(tail(&sink)["phase"].is_null());
    }
}
