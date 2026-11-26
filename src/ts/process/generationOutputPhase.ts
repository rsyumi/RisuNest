import type { GeneratingConversation } from '../storage/persistentDataStore'
import type { ConversationCommitObserver } from './conversationOperationContext'
import type { GenerationConversationOperation } from './generationConversationOperation'

export interface GenerationScriptPosition {
    refresh?(): Promise<boolean>
    index(): number | null
}

export interface GenerationOutputPhase {
    readonly phase: 'streaming' | 'processing'
    readonly messageId: string | undefined
    readonly messageIndex: number
    readonly scriptSettled: Promise<void> | null
    readonly acceptCommit: ConversationCommitObserver
    loseOutput(): void
    /** Keeps a write admitted by the save queue on this owner until it applies or fails. */
    holdWrite(): () => void
    holdStreamingPatch(): () => void
}

const key = (target: GeneratingConversation) => JSON.stringify([target.characterId, target.conversationId])
const phases = new Map<string, { state: GenerationOutputPhase; isCurrent(): boolean }>()
const scripts = new WeakMap<ConversationCommitObserver, () => () => void>()

export function getGenerationOutputPhase(target: GeneratingConversation): GenerationOutputPhase | null {
    const entry = phases.get(key(target))
    return entry?.isCurrent() ? entry.state : null
}

export function openGenerationOutputPhase(
    target: GeneratingConversation,
    initialPhase: GenerationOutputPhase['phase'],
    output: () => GenerationConversationOperation | null,
    isCurrent: () => boolean,
    acceptCommit: ConversationCommitObserver,
    onLost?: () => void,
) {
    const identity = key(target)
    let phase = initialPhase
    let invocations = 0
    let settled: Promise<void> | null = null
    let finish: (() => void) | undefined
    const writes = new Set<Promise<void>>()
    const streamingPatches = new Set<Promise<void>>()
    let reportedLoss = false
    const hold = (patch: boolean) => {
        let resolve!: () => void
        const applied = new Promise<void>(done => { resolve = done })
        writes.add(applied)
        if (patch) streamingPatches.add(applied)
        return () => {
            writes.delete(applied)
            streamingPatches.delete(applied)
            resolve()
        }
    }
    const state: GenerationOutputPhase = {
        get phase() { return phase },
        get messageId() { return output()?.messageId },
        get messageIndex() {
            const reply = output()
            return reply?.isOwned() ? reply.absoluteIndex : -1
        },
        get scriptSettled() { return phase === 'processing' ? settled : null },
        acceptCommit,
        loseOutput() {
            if (!reportedLoss && isCurrent()) {
                reportedLoss = true
                onLost?.()
            }
        },
        holdWrite: () => hold(false),
        holdStreamingPatch: () => hold(true),
    }
    const entry = { state, isCurrent }
    phases.set(identity, entry)
    scripts.set(acceptCommit, () => {
        if (invocations++ === 0) settled = new Promise<void>((resolve) => { finish = resolve })
        return () => {
            if (--invocations !== 0) return
            finish?.()
            settled = null
        }
    })
    const close = () => {
        if (phases.get(identity) === entry) phases.delete(identity)
        scripts.delete(acceptCommit)
        finish?.()
    }
    const closeAfterWrites = async () => {
        while (writes.size) await Promise.all(writes)
        close()
    }
    return {
        loseOutput: state.loseOutput,
        endStream() {
            phase = 'processing'
            if (streamingPatches.size) return Promise.all(streamingPatches).then(() => {})
        },
        close: () => writes.size ? closeAfterWrites() : close(),
    }
}

/** The observer belongs to the generation that invoked the script, including in a group chat. */
export function beginGenerationOutputScript(observer: ConversationCommitObserver | undefined) {
    return observer && scripts.get(observer)?.()
}
