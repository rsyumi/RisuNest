import type { GeneratingConversation } from '../storage/persistentDataStore'
import { ConversationSessionInactiveError } from '../storage/activeConversationSession'
import type { ConversationCommitObserver } from './conversationOperationContext'
import type { GenerationConversationOperation } from './generationConversationOperation'

export interface GenerationScriptPosition {
    refresh?(): Promise<boolean>
    index(): number | null
}

export interface GenerationOutputWrite {
    readonly phase: GenerationOutputPhase
    readonly scriptWait: Promise<void> | null
    assertIntervalOwner(): void
    failIntervalSave(error: unknown): void
    release(): void
}

export interface GenerationOutputPhase {
    readonly phase: 'streaming' | 'processing'
    readonly messageId: string | undefined
    readonly messageIndex: number
    readonly pluginWritesOpen: boolean
    readonly scriptRunning: boolean
    readonly acceptCommit: ConversationCommitObserver
    loseOutput(): void
    fail(error: unknown): void
    holdWrite(): GenerationOutputWrite
    holdStreamingPatch(): () => void
}

const key = (target: GeneratingConversation) => JSON.stringify([target.characterId, target.conversationId])
const phases = new Map<string, { state: GenerationOutputPhase; isCurrent(): boolean; checkScriptOwner(): void }>()
export interface GenerationOutputScript {
    readonly signal: AbortSignal
    fail(error: unknown): void
    assertOwner(): void
    openInterval(): () => Promise<void>
    refresh(): Promise<void>
}

const scripts = new WeakMap<ConversationCommitObserver, {
    begin(): () => void
    current(): GenerationOutputScript | undefined
}>()

export function getGenerationOutputPhase(target: GeneratingConversation): GenerationOutputPhase | null {
    const entry = phases.get(key(target))
    if (entry?.isCurrent()) return entry.state
    entry?.state.loseOutput()
    return null
}

let checkScheduled = false
export function recheckGenerationOutputOwners() {
    if (!phases.size || checkScheduled) return
    checkScheduled = true
    queueMicrotask(() => {
        checkScheduled = false
        for (const entry of phases.values()) entry.checkScriptOwner()
    })
}

export function openGenerationOutputPhase(
    target: GeneratingConversation,
    initialPhase: GenerationOutputPhase['phase'],
    output: () => GenerationConversationOperation | null,
    isCurrent: () => boolean,
    acceptCommit: ConversationCommitObserver,
    onLost?: () => void,
    generationSignal?: AbortSignal,
    refreshPosition?: () => Promise<boolean>,
) {
    const identity = key(target)
    let phase = initialPhase
    let invocations = 0
    let scriptWait: Promise<void> | null = null
    let wakeScriptWaiters: (() => void) | undefined
    const lifetime = new AbortController()
    const writes = new Map<Promise<void>, { intervalWrite: boolean; finish(): void }>()
    const streamingPatches = new Set<Promise<void>>()
    let intervalOpen = false
    let closing = false

    const updateWait = () => {
        wakeScriptWaiters?.()
        wakeScriptWaiters = undefined
        scriptWait = phase === 'processing' && invocations > 0 && !intervalOpen && !closing
            ? new Promise<void>(resolve => { wakeScriptWaiters = resolve }) : null
    }
    const hold = (patch: boolean): GenerationOutputWrite => {
        let resolve!: () => void
        const done = new Promise<void>(finish => { resolve = finish })
        const permission = { intervalWrite: intervalOpen, finish: resolve }
        writes.set(done, permission)
        if (patch) streamingPatches.add(done)
        return {
            phase: state,
            get scriptWait() { return permission.intervalWrite ? null : scriptWait },
            assertIntervalOwner() { if (permission.intervalWrite) assertOwner() },
            failIntervalSave(error) { if (permission.intervalWrite) state.fail(error) },
            release() {
                writes.delete(done)
                streamingPatches.delete(done)
                resolve()
            },
        }
    }
    const state: GenerationOutputPhase = {
        get phase() { return phase },
        get pluginWritesOpen() { return intervalOpen },
        get scriptRunning() { return phase === 'processing' && invocations > 0 && !closing },
        get messageId() { return output()?.messageId },
        get messageIndex() {
            const reply = output()
            return reply?.isOwned() ? reply.absoluteIndex : -1
        },
        acceptCommit,
        loseOutput() {
            if (lifetime.signal.aborted) return
            const report = isCurrent()
            stop()
            if (report) onLost?.()
        },
        fail(error) { if (!lifetime.signal.aborted) stop(error) },
        holdWrite: () => hold(false),
        holdStreamingPatch: () => hold(true).release,
    }
    const entry = {
        state, isCurrent,
        checkScriptOwner() {
            if (phase === 'processing' && invocations && (!isCurrent() || !output()?.isOwned())) state.loseOutput()
        },
    }
    const onAbort = () => stop(generationSignal?.reason)
    const stop = (reason: unknown = new ConversationSessionInactiveError()) => {
        closing = true
        intervalOpen = false
        updateWait()
        if (phases.get(identity) === entry) phases.delete(identity)
        scripts.delete(acceptCommit)
        generationSignal?.removeEventListener('abort', onAbort)
        lifetime.abort(reason)
        for (const permission of writes.values()) permission.finish()
        writes.clear()
        streamingPatches.clear()
    }
    const assertOwner = () => {
        if (closing || !isCurrent() || !output()?.isOwned()) {
            state.loseOutput()
            throw new ConversationSessionInactiveError()
        }
    }
    phases.get(identity)?.state.loseOutput()
    phases.set(identity, entry)
    const script: GenerationOutputScript = {
        signal: lifetime.signal,
        fail: state.fail,
        assertOwner,
        openInterval() {
            assertOwner()
            if (intervalOpen || !invocations || phase !== 'processing') throw new ConversationSessionInactiveError()
            intervalOpen = true
            // Setters that were waiting for this very script join the same drain, even if
            // the provider returns before the setter gets its next turn on the save queue.
            for (const permission of writes.values()) permission.intervalWrite = true
            updateWait()
            return async () => {
                intervalOpen = false
                updateWait()
                await Promise.all(writes.keys())
                lifetime.signal.throwIfAborted()
                assertOwner()
            }
        },
        async refresh() {
            assertOwner()
            if (refreshPosition && !await refreshPosition()) {
                state.loseOutput()
                throw new ConversationSessionInactiveError()
            }
            assertOwner()
        },
    }
    scripts.set(acceptCommit, {
        current: () => phase === 'processing' ? script : undefined,
        begin() {
            if (++invocations === 1) updateWait()
            return () => { if (--invocations === 0) updateWait() }
        },
    })
    generationSignal?.addEventListener('abort', onAbort, { once: true })
    if (generationSignal?.aborted) onAbort()
    const closeAfterWrites = async () => {
        while (writes.size && !lifetime.signal.aborted) await Promise.all(writes.keys())
        stop()
    }
    return {
        loseOutput: state.loseOutput,
        endStream() {
            phase = 'processing'
            updateWait()
            if (streamingPatches.size) return Promise.all(streamingPatches).then(() => {})
        },
        close() {
            closing = true
            if (invocations || lifetime.signal.aborted) return stop()
            return writes.size ? closeAfterWrites() : stop()
        },
    }
}

/** The observer belongs to the generation that invoked the script, including in a group chat. */
export function beginGenerationOutputScript(observer: ConversationCommitObserver | undefined) {
    return observer && scripts.get(observer)?.begin()
}

export function getGenerationOutputScript(observer: ConversationCommitObserver | undefined) {
    return observer && scripts.get(observer)?.current()
}
