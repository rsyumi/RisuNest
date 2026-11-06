import { invoke } from '@tauri-apps/api/core'
import { isTauri } from './platform'

export type GenerationDiagnosticPhase =
    | 'send-admitted' | 'history-materialization' | 'prompt-preparation'
    | 'awaiting-provider' | 'host-response' | 'semantic-output' | 'output-application' | 'checkpoint'
    | 'current-chat-parser' | 'output-trigger' | 'inlay' | 'output-listener'
    | 'tts' | 'completion-processing' | 'final-acknowledgement' | 'settled'
export type GenerationDiagnosticOutcome = 'ok' | 'failed' | 'skipped' | 'stale' | 'aborted'
export type GenerationCheckpointSkipReason =
    | 'ineligible' | 'compatibility' | 'missing-capture' | 'complete-gate'
    | 'storage-failed' | 'ownership-lost'
export type GenerationSavePath = 'windowed' | 'complete' | 'fallback' | 'unknown'
/** Why a phase exited as failed. */
export type GenerationFailureReason =
    | 'conversation-changed' | 'trigger-stop' | 'memory-failed' | 'token-limit'
    | 'fenced' | 'error' | 'stream-error'
export type GenerationLifecycleEvent =
    | 'active' | 'background' | 'start' | 'stop' | 'freeze' | 'resume'
    | 'exit' | 'expiry' | 'memory-warning' | 'trim-memory'
export type GenerationDiagnosticFallback =
    | 'none' | 'compatibility' | 'missing-capture' | 'unsupported' | 'unknown'
    | 'selected-conversation-unavailable' | 'standard-hypa-v3-disabled'
    | 'selected-conversation-not-windowed' | 'custom-or-remote-tokenizer'
    | 'experimental-hypa-v3' | 'plugin-editprocess' | 'plugin-editoutput'
    | 'plugin-chat-output' | 'generation-trigger' | 'history-regex'
    | 'old-history-lorebook-consumer' | 'additional-text-consumer'
    | 'inlay-view-consumer' | 'dynamic-history-indirection' | 'explicit-history-reference'

export interface GenerationDiagnosticAdmission {
    intent: 'normal' | 'continue' | 'reroll'
    displayMode: 'off' | 'normal' | 'strong'
    profile: 'normal' | 'low-spec'
    historyLimitEnabled: boolean
    historyLimitMultiplier?: number
    route: 'complete' | 'windowed' | 'history-window' | 'summary-aware' | 'unknown'
    fallback?: GenerationDiagnosticFallback
    admittedMessages?: number
    totalMessages?: number
}

export interface GenerationDiagnosticPhaseDetails {
    outcome?: GenerationDiagnosticOutcome
    skipReason?: GenerationCheckpointSkipReason
    resultKind?: 'buffered' | 'streaming' | 'multiline' | 'failed'
    savePath?: GenerationSavePath
    reason?: GenerationFailureReason
}

export interface GenerationPersistenceDiagnostic {
    boundary: 'checkpoint' | 'final'
    savePath: GenerationSavePath
    pendingEvents?: number
    dirtyRecords?: number
    unacknowledgedMs?: number
    expectedRevision?: number
    acknowledgedRevision?: number
}

/** What output listeners receive; counts are present once the event is built. */
export interface GenerationListenerPayloadDiagnostic {
    listeners: number
    fullHistoryListeners: number
    conversations?: number
    messages?: number
    messageCharacters?: number
}

export interface GenerationDiagnosticTrace {
    admission(value: GenerationDiagnosticAdmission): void
    phase(phase: GenerationDiagnosticPhase, edge: 'enter' | 'exit', details?: GenerationDiagnosticPhaseDetails): void
    persistence(value: GenerationPersistenceDiagnostic): void
    listenerPayload(value: GenerationListenerPayloadDiagnostic): void
}

type DiagnosticInvoke = (command: string, args: Record<string, unknown>) => Promise<unknown>

export function createGenerationDiagnostics(options: { enabled: boolean; invoke: DiagnosticInvoke; signal?: AbortSignal }) {
    let documentSequence: number | null = null
    let generationSequence = 0
    let eventSequence = 0
    let started = false
    let pendingHandshake: Promise<void> | null = null
    let handshakeUnavailable = false
    const active = () => options.enabled && !options.signal?.aborted
    const handshake = (): Promise<void> => {
        if (!active() || handshakeUnavailable) return Promise.resolve()
        if (pendingHandshake) return pendingHandshake
        try {
            pendingHandshake = options.invoke('native_generation_diagnostic', { event: { kind: 'document-start' } })
                .then((value) => {
                    pendingHandshake = null
                    if (!active()) return
                    if (typeof value === 'number' && Number.isSafeInteger(value) && value > 0) {
                        documentSequence = value
                    } else {
                        documentSequence = null
                        handshakeUnavailable = value !== 0
                    }
                }, () => {
                    pendingHandshake = null
                    documentSequence = null
                    handshakeUnavailable = true
                })
            return pendingHandshake
        } catch {
            documentSequence = null
            handshakeUnavailable = true
            return Promise.resolve()
        }
    }
    const recoverStale = async (error: unknown, event: Record<string, unknown>) => {
        if (error !== 'stale-diagnostic-document' || !active()) return
        if (documentSequence === event.documentSequence) await handshake()
        if (!active() || documentSequence === null || documentSequence === event.documentSequence) return
        try {
            // A delayed native page-start can invalidate the initial handshake.
            // Retry only this event, retaining its ordering against later phases.
            await options.invoke('native_generation_diagnostic', { event: { ...event, documentSequence } })
        } catch {}
    }
    const send = (event: Record<string, unknown>) => {
        if (!active()) return
        if (documentSequence === null) {
            if (started) void handshake()
            return
        }
        const sequenced = { ...event, documentSequence, eventSequence: ++eventSequence }
        try {
            void options.invoke('native_generation_diagnostic', { event: sequenced })
                .catch(error => recoverStale(error, sequenced))
        } catch (error) {
            void recoverStale(error, sequenced)
        }
    }
    return {
        startDocument(): void {
            if (!active() || started) return
            started = true
            void handshake()
        },
        generation(): GenerationDiagnosticTrace {
            const sequence = ++generationSequence
            const emit = (event: Record<string, unknown>) => {
                send({ ...event, generationSequence: sequence })
            }
            return {
                admission: (value) => emit({
                    kind: 'admission', intent: value.intent, displayMode: value.displayMode,
                    profile: value.profile, historyLimitEnabled: value.historyLimitEnabled,
                    historyLimitMultiplier: value.historyLimitMultiplier, route: value.route,
                    fallback: value.fallback, admittedMessages: value.admittedMessages,
                    totalMessages: value.totalMessages,
                }),
                phase: (phase, edge, details) => emit({
                    kind: 'phase', phase, edge, outcome: details?.outcome,
                    skipReason: details?.skipReason, resultKind: details?.resultKind,
                    savePath: details?.savePath, reason: details?.reason,
                }),
                persistence: (value) => emit({
                    kind: 'persistence', boundary: value.boundary, savePath: value.savePath,
                    pendingEvents: value.pendingEvents, dirtyRecords: value.dirtyRecords,
                    unacknowledgedMs: value.unacknowledgedMs,
                    expectedRevision: value.expectedRevision, acknowledgedRevision: value.acknowledgedRevision,
                }),
                listenerPayload: (value) => emit({
                    kind: 'listener-payload', listeners: value.listeners,
                    fullHistoryListeners: value.fullHistoryListeners, conversations: value.conversations,
                    messages: value.messages, messageCharacters: value.messageCharacters,
                }),
            }
        },
        lifecycle(event: GenerationLifecycleEvent, trimLevel?: number): void {
            send({ kind: 'lifecycle', event, ...(trimLevel === undefined ? {} : { trimLevel }) })
        },
    }
}

const documentLifetime = new AbortController()
if (isTauri && typeof window !== 'undefined') {
    window.addEventListener('pagehide', () => documentLifetime.abort(), { once: true })
}
export const generationDiagnostics = createGenerationDiagnostics({ enabled: isTauri, invoke, signal: documentLifetime.signal })
