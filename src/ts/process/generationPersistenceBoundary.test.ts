import 'fake-indexeddb/auto'
import { expect, it, onTestFinished, vi } from 'vitest'
import type { Chat, Database } from '../storage/database.svelte'
import { IndexedDbPersistentDataStore } from '../storage/indexedDbPersistentDataStore'
import { createPersistentDataRuntime, capturePersistentRoot, capturePersistentPresets } from '../storage/persistentDataRuntime'
import { applyGenerationResponse } from './generationResponseApplication'
import { bindWindowedGenerationController } from './generationConversationOperation'
import { cloneConversationMetadata } from '../storage/activeConversationSession'
import { createMutationGatedPersistentDataStore } from '../storage/mutationGatedPersistentDataStore'
import { createStorageMutationGate } from '../storage/storageMutationGate'
import { prepareSummaryAwareGeneration } from './summaryAwareGenerationPreparation'
import { installDisplayRecovery } from '../displayRecovery.svelte'
import { manualTimers, recoveryStore } from '../displayRecovery.testSupport'
import { createPersistenceCanonicalCapture } from '../storage/reactivePersistenceCapture.svelte'
import { createPersistentSaveObserverHarness } from '../storage/tests/persistentSaveObserverHarness.svelte'
import type { requestDataResponse } from './request/request'
import type { GenerationResponseCallbacks } from './generationResponseApplication'

vi.mock('../storage/database.svelte', () => ({ presetTemplate: {} }))
vi.mock('../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))

it('streams through the real session and save coordinator and reopens the exact completed response', async () => {
    const name = `generation-reopen-${crypto.randomUUID()}`
    const store = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
    const database = { username: 'Synthetic', botPresets: [], characters: [{
        type: 'character', chaId: 'owner', name: 'Owner', chatPage: 0,
        chats: [{ id: 'chat', name: 'Chat', message: [{ role: 'user', data: 'Prompt', chatId: 'user' }] }],
    }] } as unknown as Database
    await store.open()
    await store.replaceFromDatabase(database)
    let current = structuredClone(database)
    const runtime = createPersistentDataRuntime({
        store, clock: { setTimeout: () => Symbol('owned-clock'), clearTimeout: () => {} },
        state: {
            captureRoot: () => capturePersistentRoot(current),
            capturePresets: () => capturePersistentPresets(current),
            captureSelectedCharacter: () => current.characters[0],
            captureCharacter: (id) => current.characters.find(character => character.chaId === id) ?? null,
            getSelectedCharacterId: () => current.characters[0].chaId,
            replaceDatabase: (replacement) => { current = replacement },
            publishCharacter: (character) => { current.characters[0] = character },
            publishConversation: (_owner, chat) => { current.characters[0].chats[0] = chat },
        },
        prepareDatabase: async candidate => structuredClone(candidate),
    })
    await runtime.initializeActiveWorkingSet(database)
    const chat = () => current.characters[0].chats[0]
    const device = recoveryStore()
    const clock = manualTimers()
    await installDisplayRecovery(device.store, { timers: clock.timers })
    onTestFinished(() => installDisplayRecovery(null))
    const chunks = ['First', 'Final exact response']
    const pendingRecordWritesAtRead: number[] = []
    const applying = applyGenerationResponse({
        response: { type: 'streaming', result: new ReadableStream({
            pull(controller) {
                pendingRecordWritesAtRead.push(device.pendingWrites)
                const chunk = chunks.shift()
                if (chunk === undefined) controller.close()
                else controller.enqueue({ 0: chunk })
            },
        }, { highWaterMark: 0 }) },
        abortSignal: new AbortController().signal, continueGeneration: false,
        sayingCharacterId: 'owner', ownerCharacterId: 'owner', generationId: 'generated', generationInfo: {} as any, promptInfo: {} as any,
        removeIncompleteResponse: () => false, streamingDisplayOptimizationMode: () => 'off', ttsAutoSpeech: () => false,
        operation: {
            getCurrentSession: () => runtime.getActiveConversationSession(), getTargetChat: chat,
            isOwnerCurrent: () => true, publishTargetChat: (replacement) => { current.characters[0].chats[0] = replacement },
            invalidateSession: () => { throw new Error('Unexpected session replacement') }, incrementReloadKeys: () => {},
        },
        callbacks: {
            reformatContent: value => value, processOutput: async value => ({ data: value + ':processed', emoChanged: false }),
            runCurrentChatParser: value => value, runInlay: text => ({ text }), runOutputTrigger: async () => null,
            runOutputListeners: async () => {}, speak: async () => {}, trimIncompleteResponse: value => value,
            markResponseApplied: () => {}, onProviderFailure: message => { throw new Error(message) },
        },
    })
    // The streamed chat is in the display recovery record, stored before the stream is read.
    await vi.waitFor(() => expect(device.records).toEqual([{ working: [{ characterId: 'owner', conversationId: 'chat' }], marked: [] }]))
    device.commitAll()
    const response = await applying
    expect(pendingRecordWritesAtRead).toEqual([0, 0, 0])
    clock.runAll()
    expect(device.record).toBeNull()
    expect(response).not.toBeNull()
    expect(response!.readOutput()?.data).toBe('Final exact response:processed')
    response!.release()
    await runtime.acknowledgeGenerationCompletion()
    const reopened = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
    await reopened.open()
    const recovered = await reopened.readConversation('owner', 'chat')
    expect(recovered?.value.message.map(message => [message.chatId, message.data])).toEqual([
        ['user', 'Prompt'], ['generated', 'Final exact response:processed'],
    ])
    expect((recovered?.value as Chat).isStreaming).not.toBe(true)
})


it('records a streamed chat that has no active conversation session under the chat owner', async () => {
    const chat = { id: 'bounded-chat', message: [{ role: 'user', data: 'Prompt', chatId: 'user' }] } as unknown as Chat
    const device = recoveryStore()
    const clock = manualTimers()
    await installDisplayRecovery(device.store, { timers: clock.timers })
    onTestFinished(() => installDisplayRecovery(null))
    const recordsAtRead: unknown[] = []
    const applying = applyGenerationResponse({
        response: { type: 'streaming', result: new ReadableStream({
            pull(controller) {
                recordsAtRead.push(device.record)
                controller.enqueue({ 0: 'Streamed' })
                controller.close()
            },
        }, { highWaterMark: 0 }) },
        abortSignal: new AbortController().signal, continueGeneration: false,
        sayingCharacterId: 'group-member', ownerCharacterId: 'group', generationId: 'generated', generationInfo: {} as any, promptInfo: {} as any,
        removeIncompleteResponse: () => false, streamingDisplayOptimizationMode: () => 'off', ttsAutoSpeech: () => false,
        operation: {
            getCurrentSession: () => null, getTargetChat: () => chat,
            isOwnerCurrent: () => true, publishTargetChat: () => {},
            invalidateSession: () => {}, incrementReloadKeys: () => {},
        },
        callbacks: {
            reformatContent: value => value, processOutput: async value => ({ data: value, emoChanged: false }),
            runCurrentChatParser: value => value, runInlay: text => ({ text }), runOutputTrigger: async () => null,
            runOutputListeners: async () => {}, speak: async () => {}, trimIncompleteResponse: value => value,
            markResponseApplied: () => {}, onProviderFailure: message => { throw new Error(message) },
        },
    })
    await vi.waitFor(() => expect(device.pendingWrites).toBe(1))
    device.commitAll()
    expect((await applying)?.readOutput()?.data).toBe('Streamed')
    expect(recordsAtRead).toEqual([{ working: [{ characterId: 'group', conversationId: 'bounded-chat' }], marked: [] }])
    clock.runAll()
    expect(device.record).toBeNull()
})

it('keeps bounded preparation available through the production mutation gate and selected authority', async () => {
    const raw = new IndexedDbPersistentDataStore(`bounded-production-${crypto.randomUUID()}`, indexedDB, IDBKeyRange)
    const store = createMutationGatedPersistentDataStore(raw, createStorageMutationGate())
    const messages = Array.from({ length: 430 }, (_, index) => ({
        role: index % 2 ? 'char' : 'user', data: `Synthetic body ${index}`, chatId: `m${index}`,
    }))
    const database = { username: 'Synthetic', botPresets: [], characters: [{
        type: 'character', chaId: 'owner', name: 'Owner', chatPage: 0,
        chats: [{ id: 'chat', name: 'Chat', message: messages, hypaV3Data: {
            summaries: [{ chatMemos: messages.slice(0, 300).map(message => message.chatId), summary: 'Synthetic summary' }],
        } }],
    }] } as unknown as Database
    await store.open()
    await store.replaceFromDatabase(database)
    let current = structuredClone(database)
    const runtime = createPersistentDataRuntime({
        store, clock: { setTimeout: () => Symbol('owned-clock'), clearTimeout: () => {} },
        state: {
            captureRoot: () => capturePersistentRoot(current),
            capturePresets: () => capturePersistentPresets(current),
            captureSelectedCharacter: () => current.characters[0],
            captureCharacter: id => current.characters.find(character => character.chaId === id) ?? null,
            getSelectedCharacterId: () => current.characters[0].chaId,
            getSelectedConversationId: () => current.characters[0].chats[0].id,
            replaceDatabase: replacement => { current = replacement },
            publishCharacter: character => { current.characters[0] = character },
            publishConversation: (_owner, chat) => { current.characters[0].chats[0] = chat },
            canUseWindowedSelectedConversation: () => true,
            captureActivationRollback: () => {
                const previous = structuredClone(current)
                return () => { current = previous }
            },
        },
        prepareDatabase: async candidate => structuredClone(candidate),
    })
    await runtime.initializeActiveWorkingSet(database)
    expect(await runtime.activateCharacter('owner')).toBe(true)
    const authority = runtime.captureSelectedConversationAuthority()
    expect(authority).not.toBeNull()
    if (!authority) throw new Error('Expected production windowed authority')
    const acquireComplete = vi.spyOn(runtime, 'acquireCompleteConversation')
    const prepared = await prepareSummaryAwareGeneration({
        store, authority, conversation: current.characters[0].chats[0],
        preserveOrphanedMemory: false, isCurrent: () => {
            const latest = runtime.captureSelectedConversationAuthority()
            return latest?.sessionToken === authority.sessionToken && latest.sessionVersion === authority.sessionVersion
        },
    })
    expect(prepared.route).toBe('summary-aware')
    if (prepared.route !== 'summary-aware') throw new Error(prepared.reason)
    expect(prepared.preparation.chat.message).toEqual(messages.slice(300))
    expect(prepared.preparation.plan.effectiveMessageMemos).toEqual(messages.map(message => message.chatId))
    expect(prepared.preparation.metrics).toMatchObject({ metadataRows: 430, bodyRows: 130, bodyPages: 3 })
    expect(runtime.captureSelectedConversationAuthority()?.sessionToken).toBe(authority.sessionToken)
    expect(acquireComplete).not.toHaveBeenCalled()
    await prepared.preparation.release()
})

function syntheticConversationDatabase(): Database {
    return { username: 'Synthetic', botPresets: [], characters: [{
        type: 'character', chaId: 'owner', name: 'Owner', chatPage: 0,
        chats: [{ id: 'chat', name: 'Chat', message: [{ role: 'user', data: 'Prompt', chatId: 'user' }] }],
    }] } as unknown as Database
}

/** A complete conversation in a real store that saves only when a flush asks it to. */
async function openCompleteConversation(options: { capture: boolean }) {
    const name = `generation-checkpoint-${crypto.randomUUID()}`
    const store = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
    await store.open()
    await store.replaceFromDatabase(syntheticConversationDatabase())
    const state = createPersistentSaveObserverHarness(syntheticConversationDatabase())
    const retryDelays: number[] = []
    const backgroundErrors: unknown[] = []
    const runtime = createPersistentDataRuntime({
        store,
        clock: {
            setTimeout: (_callback: () => void, delay: number) => {
                retryDelays.push(delay)
                return Symbol('owned-clock')
            },
            clearTimeout: () => {},
        },
        onBackgroundError: (error) => { backgroundErrors.push(error) },
        state: {
            canonicalCapture: options.capture ? createPersistenceCanonicalCapture({
                root: () => state.database, presets: () => [], pluginStorage: () => null,
                character: () => state.database.characters[0], characters: () => state.database.characters,
            }) : undefined,
            captureRoot: () => capturePersistentRoot(state.database),
            capturePresets: () => capturePersistentPresets(state.database),
            captureCharacters: () => state.database.characters,
            captureSelectedCharacter: () => state.database.characters[0],
            captureCharacter: (id) => state.database.characters.find(character => character.chaId === id) ?? null,
            getSelectedCharacterId: () => state.database.characters[0].chaId,
            replaceDatabase: (replacement) => { state.database = replacement },
            publishCharacter: (character) => { state.database.characters[0] = character },
            publishConversation: (_owner, chat) => { state.database.characters[0].chats[0] = chat },
        },
        prepareDatabase: async candidate => structuredClone(candidate),
    })
    await runtime.initializeActiveWorkingSet(state.database)
    const stored = async () => {
        const reader = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
        await reader.open()
        return (await reader.readConversation('owner', 'chat'))?.value
    }
    return {
        store, state, runtime, retryDelays, backgroundErrors, stored,
        storedMessages: async () => (await stored())?.message.map(message => [message.chatId, message.data]),
        chat: () => state.database.characters[0].chats[0],
    }
}

type CompleteConversation = Awaited<ReturnType<typeof openCompleteConversation>>

function applyCheckpointedResponse(conversation: CompleteConversation, response: requestDataResponse, options: {
    callbacks?: Partial<GenerationResponseCallbacks>
    mode?: 'off' | 'balanced' | 'strong'
    abortSignal?: AbortSignal
    continueGeneration?: boolean
    isOwnerCurrent?(): boolean
    flush?(): Promise<boolean>
    clock?: ReturnType<typeof virtualClock>
} = {}) {
    return applyGenerationResponse({
        response, abortSignal: options.abortSignal ?? new AbortController().signal, continueGeneration: options.continueGeneration ?? false,
        sayingCharacterId: 'owner', ownerCharacterId: 'owner', generationId: 'generated', generationInfo: {} as any, promptInfo: {} as any,
        removeIncompleteResponse: () => false, streamingDisplayOptimizationMode: () => options.mode ?? 'off', ttsAutoSpeech: () => true,
        operation: {
            getCurrentSession: () => conversation.runtime.getActiveConversationSession(), getTargetChat: conversation.chat,
            isOwnerCurrent: options.isOwnerCurrent ?? (() => true),
            publishTargetChat: (replacement) => { conversation.state.database.characters[0].chats[0] = replacement },
            invalidateSession: () => { throw new Error('Unexpected session replacement') }, incrementReloadKeys: () => {},
        },
        callbacks: {
            reformatContent: value => value, processOutput: async value => ({ data: value + ':processed', emoChanged: false }),
            runCurrentChatParser: value => value, runInlay: text => ({ text }), runOutputTrigger: async () => null,
            runOutputListeners: async () => {}, speak: async () => {}, trimIncompleteResponse: value => value,
            markResponseApplied: () => {}, onProviderFailure: message => { throw new Error(message) },
            ...options.callbacks,
        },
        checkpoint: {
            savePath: () => conversation.runtime.getActiveConversationSession()?.residencyFallbackActive ? 'fallback' : 'complete',
            capturesImmutableCharacters: () => conversation.runtime.capturesImmutableCharacters(),
            flush: options.flush ?? (() => conversation.runtime.flushPendingDataLocallyForCheckpoint('generation-checkpoint')),
            clock: options.clock,
        },
    })
}

function virtualClock() {
    const due = new Map<symbol, { at: number, callback: () => void }>()
    const clock = {
        now: 0,
        setTimeout(callback: () => void, delay: number) {
            const handle = Symbol('checkpoint-timer')
            due.set(handle, { at: clock.now + delay, callback })
            return handle
        },
        clearTimeout(handle: unknown) { due.delete(handle as symbol) },
        advance(milliseconds: number) {
            clock.now += milliseconds
            for (const [handle, timer] of [...due]) {
                if (timer.at > clock.now || !due.has(handle)) continue
                due.delete(handle)
                timer.callback()
            }
        },
        get pending() { return due.size },
    }
    return clock
}

function gate() {
    let open!: () => void
    const opened = new Promise<void>((resolve) => { open = resolve })
    return { opened, open }
}

it.each([true, false])('saves a buffered answer before later output stages when the conversation has immutable captured values (%s)', async (capture) => {
    const conversation = await openCompleteConversation({ capture })
    const semantic = gate()
    const listener = gate()
    let semanticStarted = false
    let listenerStarted = false
    const applying = applyCheckpointedResponse(conversation, { type: 'success', result: 'Buffered answer' }, { callbacks: {
        processOutput: async (value) => {
            semanticStarted = true
            await semantic.opened
            return { data: value + ':processed', emoChanged: false }
        },
        runOutputListeners: async () => {
            listenerStarted = true
            await listener.opened
        },
    } })
    await vi.waitFor(() => expect(semanticStarted).toBe(true))
    // Nothing is saved before the first application, so this window stays unrecoverable.
    expect(await conversation.storedMessages()).toEqual([['user', 'Prompt']])
    semantic.open()
    await vi.waitFor(() => expect(listenerStarted).toBe(true))
    expect(await conversation.storedMessages()).toEqual(capture
        ? [['user', 'Prompt'], ['generated', 'Buffered answer:processed']]
        : [['user', 'Prompt']])
    listener.open()
    const response = await applying
    response!.release()
    await conversation.runtime.acknowledgeGenerationCompletion()
    expect(await conversation.storedMessages()).toEqual([['user', 'Prompt'], ['generated', 'Buffered answer:processed']])
})

it('runs every later output stage once after a checkpoint storage failure and saves the answer at completion', async () => {
    const conversation = await openCompleteConversation({ capture: true })
    const commit = vi.spyOn(conversation.store, 'commit').mockRejectedValueOnce(new Error('synthetic I/O failure'))
    const stages: string[] = []
    const response = await applyCheckpointedResponse(conversation, { type: 'success', result: 'Buffered answer' }, { callbacks: {
        runCurrentChatParser: (chat) => {
            stages.push('parser')
            return chat
        },
        runOutputTrigger: async () => {
            stages.push('trigger')
            return null
        },
        runOutputListeners: async () => { stages.push('listener') },
        speak: async () => { stages.push('tts') },
    } })
    expect(stages).toEqual(['tts', 'parser', 'trigger', 'listener'])
    expect(conversation.backgroundErrors).toHaveLength(1)
    expect(conversation.retryDelays).toContain(2_000)
    expect(await conversation.storedMessages()).toEqual([['user', 'Prompt']])
    expect(conversation.runtime.pendingConversationMutationDiagnostics()).toEqual({ pendingEvents: 1, unacknowledgedMs: expect.any(Number) })
    response!.release()
    await conversation.runtime.acknowledgeGenerationCompletion()
    expect(conversation.runtime.pendingConversationMutationDiagnostics()).toEqual({ pendingEvents: 0, unacknowledgedMs: undefined })
    expect(commit).toHaveBeenCalledTimes(2)
    expect(conversation.backgroundErrors).toHaveLength(1)
    expect(await conversation.storedMessages()).toEqual([['user', 'Prompt'], ['generated', 'Buffered answer:processed']])
})

it.each(['off', 'balanced', 'strong'] as const)('saves continuously streamed output at the interval in %s mode and once more before completion processing', async (mode) => {
    const conversation = await openCompleteConversation({ capture: true })
    const clock = virtualClock()
    const events: string[] = []
    let saving = 0
    let readDuringSave = false
    let text = ''
    let chunks = 0
    const stream = new ReadableStream({
        pull(controller) {
            if (saving > 0) readDuringSave = true
            if (chunks === 50) return controller.close()
            text += `chunk-${chunks++} `
            clock.advance(100)
            controller.enqueue({ 0: text })
        },
    }, { highWaterMark: 0 })
    const response = await applyCheckpointedResponse(conversation, { type: 'streaming', result: stream }, {
        mode, clock,
        flush: async () => {
            saving++
            events.push(`request at ${clock.now}`)
            try {
                const saved = await conversation.runtime.flushPendingDataLocallyForCheckpoint('generation-checkpoint')
                const stored = (await conversation.storedMessages())?.[1]?.[1]
                events.push(`saved ${stored === text ? 'latest raw' : stored === `${text}:processed` ? 'latest processed' : 'older output'}`)
                return saved
            } finally {
                saving--
            }
        },
        callbacks: {
            processOutput: async (value) => {
                if (chunks === 50 && value === text) events.push('final semantic pass')
                return { data: value + ':processed', emoChanged: false }
            },
        },
    })
    expect(response).not.toBeNull()
    // The output never pauses, so the first save comes from the interval rather than a quiet gap.
    const firstRequest = Number(events[0].replace('request at ', ''))
    expect(firstRequest).toBeGreaterThanOrEqual(2_000)
    expect(firstRequest).toBeLessThan(5_000)
    expect(readDuringSave).toBe(true)
    expect(clock.pending).toBe(0)
    // Strong mode saves its preview before the deferred final pass; the others save the processed output.
    const completion = events.indexOf('request at 5000')
    expect(events.slice(completion)).toEqual(mode === 'strong'
        ? ['request at 5000', 'saved latest raw', 'final semantic pass']
        : ['request at 5000', 'saved latest processed'])
    if (mode !== 'strong') expect(events.indexOf('final semantic pass')).toBeLessThan(completion)
    if (mode !== 'strong') expect((await conversation.storedMessages())?.[1]).toEqual(['generated', `${text}:processed`])
    response!.release()
    await conversation.runtime.acknowledgeGenerationCompletion()
    expect(await conversation.storedMessages()).toEqual([['user', 'Prompt'], ['generated', `${text}:processed`]])
})

it.each(['abort', 'ownership loss'] as const)('stops streamed checkpoints at %s', async (ending) => {
    const conversation = await openCompleteConversation({ capture: true })
    const clock = virtualClock()
    const abort = new AbortController()
    let owned = true
    const requests: number[] = []
    let text = ''
    let chunks = 0
    const stream = new ReadableStream({
        pull(controller) {
            if (chunks === 50) return controller.close()
            if (chunks === 30) {
                if (ending === 'abort') abort.abort()
                else owned = false
            }
            text += `chunk-${chunks++} `
            clock.advance(100)
            controller.enqueue({ 0: text })
        },
    }, { highWaterMark: 0 })
    const response = await applyCheckpointedResponse(conversation, { type: 'streaming', result: stream }, {
        clock, abortSignal: abort.signal, isOwnerCurrent: () => owned,
        flush: () => {
            requests.push(clock.now)
            return conversation.runtime.flushPendingDataLocallyForCheckpoint('generation-checkpoint')
        },
    })
    expect(response).toBeNull()
    expect(requests).toEqual([2100])
    expect(clock.pending).toBe(0)
})

it.each(['append', 'continue', 'abort'] as const)('keeps the session out of compatibility mode when saves land around the streaming state (%s)', async (ending) => {
    const conversation = await openCompleteConversation({ capture: true })
    const session = () => conversation.runtime.getActiveConversationSession()!
    // A start trigger's variable leaves a metadata event pending before the response.
    session().updateChatVariables(new Map([['mood', 'calm']]))
    const saves: Array<{ fallback: boolean, unacknowledged: number, streaming: boolean | undefined }> = []
    const save = async () => {
        await conversation.runtime.flushPendingData('synthetic-save')
        saves.push({
            fallback: session().residencyFallbackActive,
            unacknowledged: session().version - session().persistedVersion,
            streaming: conversation.chat().isStreaming,
        })
    }
    const abort = new AbortController()
    let reads = 0
    const stream = new ReadableStream({
        async pull(controller) {
            reads++
            if (reads === 1) {
                // Between the response append and the first streamed edit.
                await save()
                controller.enqueue({ 0: 'Partial' })
            } else if (ending === 'abort') {
                abort.abort()
            } else {
                controller.enqueue({ 0: 'Partial answer' })
                controller.close()
            }
        },
    }, { highWaterMark: 0 })
    const response = await applyCheckpointedResponse(conversation, { type: 'streaming', result: stream }, {
        abortSignal: abort.signal,
        continueGeneration: ending === 'continue',
        callbacks: { runOutputTrigger: async () => { await save(); return null } },
    })
    response?.release()
    await save()

    expect(response === null).toBe(ending === 'abort')
    expect(saves.map(({ streaming }) => streaming)).toEqual(ending === 'abort' ? [true, false] : [true, false, false])
    expect(saves.every(({ fallback, unacknowledged }) => !fallback && unacknowledged === 0)).toBe(true)
    expect(conversation.backgroundErrors).toEqual([])
})

it('clears the streaming state of a windowed conversation when its stream is stopped', async () => {
    const store = new IndexedDbPersistentDataStore(`windowed-stop-${crypto.randomUUID()}`, indexedDB, IDBKeyRange)
    const database = syntheticConversationDatabase()
    await store.open()
    await store.replaceFromDatabase(database)
    let current = structuredClone(database)
    const runtime = createPersistentDataRuntime({
        store, clock: { setTimeout: () => Symbol('owned-clock'), clearTimeout: () => {} },
        state: {
            captureRoot: () => capturePersistentRoot(current),
            capturePresets: () => capturePersistentPresets(current),
            captureSelectedCharacter: () => current.characters[0],
            captureCharacter: id => current.characters.find(character => character.chaId === id) ?? null,
            getSelectedCharacterId: () => current.characters[0].chaId,
            getSelectedConversationId: () => current.characters[0].chats[0].id,
            replaceDatabase: replacement => { current = replacement },
            publishCharacter: character => { current.characters[0] = character },
            publishConversation: (_owner, chat) => { current.characters[0].chats[0] = chat },
            canUseWindowedSelectedConversation: () => true,
        },
        prepareDatabase: async candidate => structuredClone(candidate),
    })
    await runtime.initializeActiveWorkingSet(database)
    expect(await runtime.activateCharacter('owner')).toBe(true)
    const target = runtime.captureSelectedConversationTarget()!
    const authority = runtime.captureSelectedConversationAuthority()!
    const windowChat = { ...cloneConversationMetadata(current.characters[0].chats[0]), message: [] } as Chat
    const controller = runtime.captureWindowedConversationMutationController(target, windowChat, authority.totalMessages)!
    onTestFinished(bindWindowedGenerationController(windowChat, controller))
    const abort = new AbortController()
    let reads = 0
    const applying = applyGenerationResponse({
        response: { type: 'streaming', result: new ReadableStream({
            pull(controller) {
                if (++reads === 1) controller.enqueue({ 0: 'Partial' })
                else abort.abort()
            },
        }, { highWaterMark: 0 }) },
        abortSignal: abort.signal, continueGeneration: false,
        sayingCharacterId: 'owner', ownerCharacterId: 'owner', generationId: 'generated', generationInfo: {} as any, promptInfo: {} as any,
        removeIncompleteResponse: () => false, streamingDisplayOptimizationMode: () => 'off', ttsAutoSpeech: () => false,
        operation: {
            getCurrentSession: () => null, getTargetChat: () => windowChat,
            isOwnerCurrent: () => true, publishTargetChat: () => {},
            invalidateSession: () => {}, incrementReloadKeys: () => {},
        },
        callbacks: {
            reformatContent: value => value, processOutput: async value => ({ data: value, emoChanged: false }),
            runCurrentChatParser: value => value, runInlay: text => ({ text }), runOutputTrigger: async () => null,
            runOutputListeners: async () => {}, speak: async () => {}, trimIncompleteResponse: value => value,
            markResponseApplied: () => {}, onProviderFailure: message => { throw new Error(message) },
        },
    })

    expect(await applying).toBeNull()
    expect(current.characters[0].chats[0].isStreaming).not.toBe(true)
    await runtime.flushPendingData('windowed-stop')
    expect(runtime.getSelectedConversationMode()).toBe('windowed')
    expect((await store.readConversation('owner', 'chat'))?.value.message.map(message => message.data)).toEqual(['Prompt', 'Partial'])
})
