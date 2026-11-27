import { get } from 'svelte/store'
import { activeRerollConversations } from '../durableReroll'
import { isGenerationRequestPhaseOpen, trackConversationPatch } from '../process/generationRequestPhase'
import { getGenerationOutputPhase, type GenerationOutputPhase } from '../process/generationOutputPhase'
import { doingChat } from '../process/index.svelte'
import {
    acquireCompleteConversation,
    captureSelectedConversationTarget,
    captureSelectedConversationAuthority,
    commitPreparedUnitIntent,
    flushPendingDataLocally,
    getActiveConversationSession,
    getPersistentRevision,
    getPersistentDataRuntime,
} from '../storage/persistentDataRuntime.svelte'
import type { Chat, Message } from '../storage/database.svelte'
import type { SelectedConversationTarget } from '../storage/activeWorkingSet.svelte'
import type { PersistentDataRuntime } from '../storage/persistentDataRuntime'
import { cloneConversationMetadata } from '../storage/selectedConversationLifecycle'
import { assertPinnedRevision } from '../storage/persistentRecordIterator'
import { valuesEqual } from '../process/conversationOperationContext'
import { safeStructuredClone } from '../safeStructuredClone'
import { DBState, selectedCharID } from '../stores.svelte'
import { createConversationPatchAccess } from './conversationPatchAccess'
import { livePatchSource, loadPatchMessages, planConversationPatch, prepareConversationPatchCommit,
    type ConversationPatchPlan, type ConversationPatchRequest, type ConversationPatchResult } from './conversationPatch'
import { applyGenerationConversationRanges } from './generationConversationOwner'
import { throwIfAborted } from './pluginQueryInput'

function getSelectedConversation() {
    const character = DBState.db.characters[get(selectedCharID)]
    return character?.chats[character.chatPage ?? 0] ?? null
}

// Only fields the patch changed are published. A chunk or a script can update another
// field while the expected-revision commit is in flight.
function publishFields<T extends object>(before: T, after: T, current: T): T {
    const result = { ...current }
    for (const key of new Set([...Object.keys(before), ...Object.keys(after)]) as Set<keyof T>) {
        if (valuesEqual(before[key], after[key]) || !valuesEqual(current[key], before[key])) continue
        if (Object.hasOwn(after, key)) result[key] = after[key]
        else delete result[key]
    }
    return result
}

function captureStreamingPublication(
    runtime: PersistentDataRuntime, target: SelectedConversationTarget, phase: GenerationOutputPhase,
    shell: Chat, planned: Extract<ConversationPatchPlan, { kind: 'apply' }>, message: (index: number) => Message,
) {
    const session = runtime.getActiveConversationSession()
    const overlay = runtime.captureSelectedConversationAuthority()?.overlay
    const authorityEpoch = runtime.getStorageAuthorityEpoch()
    const previousVariables = { ...shell.scriptstate }
    const rows = planned.runs.flatMap(run => run.messages.map((after, offset) => {
        const index = run.start + offset
        const before = safeStructuredClone(message(index))
        return { index, before, after, binding: overlay?.bind(index, before) }
    }))
    const ids = session ? rows.flatMap(row => row.before.chatId ? [row.before.chatId] : []) : []
    const occurrences = new Map(ids.map(id => [id, 0]))
    if (occurrences.size) {
        for (const message of session!.materializeCompatibilityArray()) {
            const id = message.chatId
            if (id && occurrences.has(id)) occurrences.set(id, occurrences.get(id)! + 1)
        }
    }
    const unsubscribe = session?.subscribe(event => {
        // A local insertion before a patched row moves its durable edit with it. Inside
        // a replaced span a unique surviving ID is the only proof of a move.
        for (const edit of event?.mutations ?? []) {
            for (const row of rows) {
                if (row.index < edit.start) continue
                if (row.index >= edit.start + edit.deleteCount) {
                    row.index += edit.messages.length - edit.deleteCount
                } else if (edit.deleteCount === 1 && edit.messages.length === 1 && edit.messages[0].chatId === row.before.chatId) {
                    continue
                } else {
                    const id = row.before.chatId
                    const found = id ? edit.messages.findIndex(item => item.chatId === id) : -1
                    row.index = id && occurrences.get(id) === 1 && found >= 0 && edit.messages.findLastIndex(item => item.chatId === id) === found
                        ? edit.start + found : -1
                }
            }
        }
    })
    const releaseWrite = phase.holdStreamingPatch()
    return {
        release() {
            unsubscribe?.()
            for (const row of rows) if (row.binding) overlay!.release(row.binding)
            releaseWrite()
        },
        publish() {
            const current = getSelectedConversation()
            const currentTarget = runtime.captureSelectedConversationTarget()
            const currentSession = runtime.getActiveConversationSession()
            if (!current || !currentTarget || currentTarget.navigationGeneration !== target.navigationGeneration ||
                currentTarget.characterId !== target.characterId || current.id !== target.conversationId ||
                runtime.getStorageAuthorityEpoch() !== authorityEpoch || currentSession !== session ||
                (!session && runtime.captureSelectedConversationAuthority()?.overlay !== overlay)) return
            const currentSource = runtime.getActiveConversationViewportSource()
            const ranges = rows.flatMap(row => {
                const index = row.binding ? overlay!.locate(row.binding) : row.index
                if (index === null || index < 0) return []
                const body = currentSession?.materializeCompatibilityArray()[index] ?? currentSource?.projectedLocalRow?.(index)
                // A clean windowed row is already provided by the committed revision.
                if (!body || body.chatId !== row.before.chatId) return []
                const patched = publishFields(row.before, row.after, body)
                return valuesEqual(body, patched) ? [] : [{ start: index, before: [body], messages: [patched] }]
            }).sort((a, b) => a.start - b.start)
            const metadata = cloneConversationMetadata(current)
            const updated = planned.scriptstate ? { ...metadata, scriptstate:
                publishFields<Record<string, unknown>>(previousVariables, planned.scriptstate, current.scriptstate ?? {}) } : metadata
            const generation = getGenerationOutputPhase(currentTarget) ?? phase
            if (!applyGenerationConversationRanges(currentTarget, current, generation, ranges, updated, runtime)) {
                throw new Error('Committed conversation patch could not be published to its owner')
            }
        },
    }
}

async function commitStreamingPatch(request: ConversationPatchRequest, signal?: AbortSignal, intervalOwner?: GenerationOutputPhase): Promise<ConversationPatchResult | null> {
    const runtime = getPersistentDataRuntime()
    let outcome: ConversationPatchResult | null = null
    let publication: ReturnType<typeof captureStreamingPublication> | undefined
    const releasePublication = () => {
        publication?.release()
        publication = undefined
    }
    // The queue flushes pending structure before each attempt. New chunks can still update
    // bodies while we read. A structural change needs a fresh flush and a new positional plan.
    let retry = false
    do {
        retry = false
        let committed = false
        const revision = await runtime.commitPreparedUnitIntent('plugin-conversation-patch', async reader => {
            releasePublication()
            outcome = null
            throwIfAborted(signal)
            const phase = getGenerationOutputPhase(request)
            if (!phase) return null
            const busy = { status: 'busy', revision: reader.revision } as const
            // The interval can close while this admitted patch is queued. Its caller holds
            // admission through publication, and the script waits for that caller before resuming.
            const admitted = () => getGenerationOutputPhase(request) === phase &&
                (intervalOwner ? phase === intervalOwner : phase.phase === 'streaming')
            if (!admitted()) { outcome = busy; return null }
            const target = runtime.captureSelectedConversationTarget()
            const shell = getSelectedConversation()
            if (!target || !shell || target.characterId !== request.characterId || target.conversationId !== request.conversationId) {
                outcome = busy
                return null
            }
            const session = runtime.getActiveConversationSession()
            const source = runtime.getActiveConversationViewportSource()
            const snapshot = source?.snapshot()
            const metadata = await reader.readConversationMetadata(request.characterId, request.conversationId)
            if (!metadata) {
                const current = getGenerationOutputPhase(request)
                if (admitted()) {
                    outcome = { status: 'conflict', conflict: { target: 'conversation', reason: 'not-found' }, revision: reader.revision }
                } else if (current?.phase === 'processing') outcome = busy
                return null
            }
            assertPinnedRevision(reader.revision, metadata.revision, 'Streaming patch conversation')
            const stored = await loadPatchMessages(reader, request, metadata.value.totalMessages)
            throwIfAborted(signal)
            if (!admitted()) {
                outcome = getGenerationOutputPhase(request)?.phase === 'processing' ? busy : null
                return null
            }
            const messages = session?.materializeCompatibilityArray()
            if (getSelectedConversation() !== shell || source !== runtime.getActiveConversationViewportSource() ||
                (!session && snapshot?.version !== source?.snapshot().version)) {
                retry = true
                return null
            }
            if (!messages) {
                const overlay = runtime.captureSelectedConversationAuthority()?.overlay
                if (!overlay) { outcome = busy; return null }
                // An insertion, removal or rewrite elsewhere can change even an indexed ID
                // lookup that did not read that position. Settle it before using stored anchors.
                const aligned = overlay.captureProjection().intervals(0, overlay.count).every(interval =>
                    'storedStart' in interval ? interval.start === interval.storedStart :
                        interval.messages.length === 1 && interval.start === phase.messageIndex &&
                        !!phase.messageId && interval.messages[0].chatId === phase.messageId)
                if (!aligned) { retry = true; return null }
            }
            const message = (index: number) => messages?.[index] ?? source?.projectedLocalRow?.(index) ?? stored.message(index)
            const owner = messages ? livePatchSource(request, messages, shell.scriptstate) : {
                ...stored, totalMessages: source!.snapshot().totalMessages, message, scriptstate: shell.scriptstate,
            }
            if (!stored.matchesLive(owner, phase.messageIndex, phase.messageId)) {
                retry = true
                return null
            }
            const planned = planConversationPatch(request, owner)
            if (planned.kind === 'conflict') {
                outcome = { status: 'conflict', conflict: planned.conflict, revision: reader.revision }
                return null
            }
            publication = captureStreamingPublication(runtime, target, phase, shell, planned, message)
            return prepareConversationPatchCommit(request, planned)
        }, async () => {
            publication?.publish()
            releasePublication()
        }, {
            committed: () => { committed = true },
            retire: releasePublication,
            // A callback may catch a rejected save. Its suspended script still needs to stop.
            saveFailed: intervalOwner?.fail,
        }).finally(() => {
            // A durable write whose first projection failed is still owned by the save
            // queue. Retry must publish to the same rows before the generation closes.
            if (!committed) releasePublication()
        })
        if (revision !== null) return { status: 'applied', revision }
    } while (retry)
    return outcome
}

/** Conversation patches against the running app, shared by every plugin. */
export const conversationPatchAccess = createConversationPatchAccess({
    commitStreamingPatch,
    flushPendingData: (reason) => flushPendingDataLocally(reason),
    getPersistentRevision: () => getPersistentRevision(),
    captureSelectedConversationTarget: () => captureSelectedConversationTarget(),
    acquireCompleteConversation: (reason, target) => acquireCompleteConversation(reason, target),
    getActiveConversationSession: () => getActiveConversationSession(),
    captureSelectedConversationAuthority: () => captureSelectedConversationAuthority(),
    getSelectedConversation,
    isConversationGenerating: (target) => {
        if (get(activeRerollConversations).includes(target.conversationId)) return true
        if (!get(doingChat)) return false
        const selected = captureSelectedConversationTarget()
        return selected?.characterId === target.characterId && selected.conversationId === target.conversationId
    },
    isGenerationRequestPhaseOpen: (target) => isGenerationRequestPhaseOpen(target),
    trackConversationPatch: (target) => trackConversationPatch(target),
    commitPreparedUnitIntent: (reason, prepare) => commitPreparedUnitIntent(reason, prepare),
})
