import { mkdir, writeFile, readFile } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { createHash } from 'node:crypto'
import { performance } from 'node:perf_hooks'
import { resolve } from 'node:path'
import { afterAll, beforeAll, describe, expect, it, vi } from 'vitest'
import { SaveCoordinator, type WindowedConversationPersistenceAuthority } from '../../src/ts/storage/saveCoordinator'
import { ActiveConversationSession } from '../../src/ts/storage/activeConversationSession'
import { WindowedConversationOverlay } from '../../src/ts/storage/windowedConversationOverlay'
import { createPersistenceCanonicalCapture } from '../../src/ts/storage/reactivePersistenceCapture.svelte'
import { captureRoot as capturePersistentRoot } from '../../src/ts/storage/saveCoordinator.testSupport'
import { createPersistentStoreHarness } from '../../tests/support/persistentStoreHarness'
import { prepareHistoryWindow, createStoreHistoryWindowReader } from '../../src/ts/process/historyWindowGeneration'
import { runCurrentChatParserPass } from '../../src/ts/process/currentChatParserPass'
import { createStreamingDisplayController } from '../../src/ts/process/streamingDisplayScheduler'
import { dispatchChatOutputListeners } from '../../src/ts/plugins/pluginChatOutputListeners'
import { createProductionPluginChatOutputProjector } from '../../src/ts/plugins/pluginDatabaseAccess'
import type { Message } from '../../src/ts/storage/database.svelte'
import type { WorkingSetCommit, PersistentDataStore } from '../../src/ts/storage/persistentDataStore'
import { safeStructuredClone } from '../../src/ts/safeStructuredClone'
import { reactiveFixture } from './reactiveFixture.svelte'
import { historyFixture, bufferedResponse, streamingResponse, countTokens } from './fixture'

const startup = vi.hoisted(() => ({ roots: [] as Array<() => void>, disposers: [] as Array<() => void> }))
// Vitest's asynchronous graph evaluation can execute roots before cyclic exports exist.
// Defer framework startup until imports settle, then execute the original roots before measuring.
vi.mock('svelte/internal/client', async (original) => {
    const actual = await original<typeof import('svelte/internal/client')>()
    return { ...actual, effect_root: (fn: () => void) => {
        let dispose: (() => void) | undefined
        startup.roots.push(() => { dispose = actual.effect_root(fn); startup.disposers.push(dispose) })
        return () => dispose?.()
    } }
})
beforeAll(async () => { for (const root of startup.roots.splice(0)) root(); await Promise.resolve() })
const probe = vi.hoisted(() => {
    const empty = () => ({ cloneCalls: 0, clonedHistoryCharacters: 0, clonedResponseCharacters: 0,
        jsonStringifyCalls: 0, serializedHistoryCharacters: 0, serializedResponseCharacters: 0, messageComparisons: 0, conversationPrefixComparisons: 0, conversationEqualityComparisons: 0 })
    const volume = (value: unknown, seen = new Set<object>()): [number, number] => {
        if (typeof value === 'string') return value.startsWith('synthetic-history') ? [value.length, 0]
            : value.startsWith('synthetic-response') ? [0, value.length] : [0, 0]
        if (!value || typeof value !== 'object' || seen.has(value)) return [0, 0]
        seen.add(value)
        let history = 0, response = 0
        for (const part of Object.values(value)) { const [h, r] = volume(part, seen); history += h; response += r }
        return [history, response]
    }
    return { enabled: false, counters: empty(), empty, volume, store: null as unknown }
})
vi.mock('../../src/ts/storage/persistentDataStoreFactory', () => ({ getPersistentDataStore: () => probe.store as PersistentDataStore }))

const evidence: Record<string, unknown>[] = []
const withVolume = process.env.GENERATION_MEMORY_VOLUME === '1'
const smoke = process.env.GENERATION_MEMORY_SMOKE === '1'
const response = bufferedResponse()
const selected = process.env.GENERATION_MEMORY_GROUP ?? 'all'
const originalStringify = JSON.stringify
const originalClone = globalThis.structuredClone
const comparisonGlobal = globalThis as unknown as { __generationMemoryComparison?: (name: string) => void }
comparisonGlobal.__generationMemoryComparison = (name) => {
    if (!probe.enabled) return
    if (name === 'messageMatchesAfterIdNormalization') probe.counters.messageComparisons++
    else if (name === 'conversationMatchesAfterIdNormalization') probe.counters.conversationPrefixComparisons++
    else if (name === 'conversationJsonEquals') probe.counters.conversationEqualityComparisons++
}
JSON.stringify = ((value: unknown, ...args: unknown[]) => {
    if (probe.enabled) { const [h, r] = probe.volume(value); probe.counters.jsonStringifyCalls++;
        probe.counters.serializedHistoryCharacters += h; probe.counters.serializedResponseCharacters += r }
    return originalStringify(value, ...args as [])
}) as typeof JSON.stringify
globalThis.structuredClone = ((value: unknown, ...args: unknown[]) => {
    if (probe.enabled) { const [h, r] = probe.volume(value); probe.counters.cloneCalls++;
        probe.counters.clonedHistoryCharacters += h; probe.counters.clonedResponseCharacters += r }
    return originalClone(value, ...args as [])
}) as typeof structuredClone
const overlaySource = 'src/ts/storage/windowedConversationOverlay.ts'
const hasOverlay = !process.env.GENERATION_MEMORY_BASELINE_SOURCE ||
    existsSync(resolve(process.env.GENERATION_MEMORY_BASELINE_SOURCE, overlaySource))
const files = ['src/ts/storage/saveCoordinator.ts', 'src/ts/storage/reactivePersistenceCapture.svelte.ts',
    'src/ts/storage/activeConversationSession.ts', 'src/ts/storage/segmentedConversationResidency.ts',
    ...(hasOverlay ? [overlaySource] : []),
    'src/ts/process/currentChatParserPass.ts', 'src/ts/plugins/pluginDatabaseAccess.ts',
    'src/ts/process/streamingDisplayScheduler.ts']

async function phase<T>(name: string, operation: () => T | Promise<T>) {
    probe.counters = probe.empty()
    probe.enabled = withVolume
    const started = performance.now()
    let expected = performance.now(), maximumDelay = 0
    const timer = setInterval(() => { const now = performance.now(); maximumDelay = Math.max(maximumDelay, now - expected - 5); expected = now }, 5)
    try {
        const value = await operation()
        const elapsedMs = performance.now() - started
        await new Promise<void>((done) => setTimeout(done, 0))
        return { value, metric: { name, elapsedMs, eventLoopDelayMs: Math.max(0, maximumDelay),
            volumeInstrumentation: withVolume, ...(withVolume ? probe.counters : {}), rendererPeakBytes: null } }
    } finally { clearInterval(timer); probe.enabled = false }
}

function clock() {
    let now = 0, next = 0
    const timers = new Map<number, { at: number; callback: () => void }>()
    return { now: () => now,
        setTimeout(callback: () => void, delay: number) { const id = ++next; timers.set(id, {at: now + delay, callback}); return id as unknown as ReturnType<typeof setTimeout> },
        clearTimeout(id: unknown) { timers.delete(id as number) },
        async advance(ms: number) { now += ms; for (const [id, item] of timers) if (item.at <= now) { timers.delete(id); item.callback() }
            await Promise.resolve(); await Promise.resolve() },
        clear() { timers.clear() },
    }
}

async function setup(tokens: number, route: 'complete' | 'windowed', metadataBytes = 0) {
    const fixture = historyFixture(tokens, metadataBytes)
    const storage = await createPersistentStoreHarness(fixture.database)
    probe.store = storage.store
    const metrics: unknown[] = []
    const state = reactiveFixture(fixture.database)
    const database = state.database, character = database.characters[0], chat = character.chats[0]
    const overlay = route === 'windowed' && hasOverlay
        ? new WindowedConversationOverlay(character.chaId, chat.id!, storage.revision, chat.message.length)
        : undefined
    let authority: WindowedConversationPersistenceAuthority | null = null
    let session!: ActiveConversationSession
    const timer = clock()
    let captureAvailable = route !== 'complete'
    const canonicalCapture = createPersistenceCanonicalCapture({ root: () => database,
        pluginStorage: () => null, presets: () => database.botPresets, character: () => captureAvailable ? character : null,
        characters: () => captureAvailable && route === 'complete' ? database.characters : [] })
    const commits: { elapsedMs: number; jsonUtf8Bytes: number; operations: number }[] = []
    let blocked: Promise<void> | null = null, enter: (() => void) | null = null
    const realCommit = storage.store.commit.bind(storage.store)
    storage.store.commit = async (input: WorkingSetCommit) => {
        const jsonUtf8Bytes = Buffer.byteLength(originalStringify(input))
        if (blocked) { enter?.(); await blocked; blocked = null }
        const start = performance.now()
        const result = await realCommit(input)
        commits.push({elapsedMs: performance.now() - start, jsonUtf8Bytes, operations: input.conversations?.length ?? 0})
        return result
    }
    const coordinator = new SaveCoordinator({ store: storage.store, canonicalCapture,
        captureRoot: () => capturePersistentRoot(database), captureCharacters: () => captureAvailable && route === 'complete' ? database.characters : [],
        captureSelectedCharacter: () => captureAvailable ? character : null, captureCharacter: (id) => captureAvailable && id === character.chaId ? character : null,
        captureSelectedConversationAuthority: () => authority, replaceDatabase: () => { throw new Error('Unexpected replacement') }, clock: timer,
        onConversationMutationPersistenceStarted: (event) => session.beginPersistence(event.sessionVersion),
        onConversationMutationPersisted: (event) => { session.acknowledgePersisted(event.sessionToken, event.sessionVersion, event.revision)
            if (authority) authority = { ...authority, storeRevision: event.revision, persistedSessionVersion: event.sessionVersion } },
        onWindowedSelectedConversationRevision: (revision, totalMessages) => { if (authority) authority = { ...authority, storeRevision: revision, totalMessages: totalMessages ?? authority.totalMessages } },
    })
    coordinator.initialize(storage.revision)
    session = new ActiveConversationSession({characterId: character.chaId, conversationId: chat.id!, conversation: chat,
        storeRevision: storage.revision, onMutation: (event) => {
            if (authority) authority = { ...authority, sessionVersion: event.sessionVersion, totalMessages: chat.message.length }
            coordinator.recordActiveConversationMutation(event)
        } })
    if (route === 'complete') {
        captureAvailable = true
        metrics.push((await phase('initial-canonical-capture', () => canonicalCapture.materializedCharacters!())).metric)
        metrics.push((await phase('promotion-baseline-adoption', () => {
            expect(coordinator.adoptHydratedCharacter(storage.revision, coordinator.mutationGeneration, character)).toBe(true)
        })).metric)
    } else {
        authority = { kind: 'windowed', characterId: character.chaId, conversationId: chat.id!,
            sessionToken: session.sessionToken, storeRevision: storage.revision,
            persistedSessionVersion: 0, sessionVersion: 0, totalMessages: chat.message.length, overlay }
        expect(coordinator.adoptWindowedSelectedConversation(storage.revision, coordinator.mutationGeneration, character, authority)).toBe(true)
    }
    const projectionDurations: unknown[] = [], rebaseDurations: unknown[] = []
    const owner = overlay ?? coordinator
    const measured = owner as unknown as Record<string, (...args: unknown[]) => unknown>
    const projectionMethod = overlay ? 'plan' : route === 'complete' ? 'projectConversationMutations' : 'projectWindowedConversationMutations'
    for (const [name, values] of [[projectionMethod, projectionDurations],
        [overlay ? 'acknowledge' : 'acknowledgeConversationMutations', rebaseDurations]] as const) {
        const original = measured[name].bind(owner)
        measured[name] = (...args: unknown[]) => { const start = performance.now(); const before = {...probe.counters}; const result = original(...args)
            const record = () => values.push({elapsedMs: performance.now() - start,
                ...(withVolume ? Object.fromEntries(Object.entries(probe.counters).map(([key, value]) => [key, value - before[key as keyof typeof before]])) : {}),
                comparisonInstrumentation: process.env.GENERATION_MEMORY_COMPARISONS === '1'})
            if (result instanceof Promise) return result.finally(record)
            record(); return result }
    }
    function retention() {
        const pending = (coordinator as unknown as {pendingConversationMutations: {event: {mutations: {messages: Message[]}[]}}[]}).pendingConversationMutations
        const dirty = (session as unknown as {residency: {dirtyRecords: {messages: Message[]}[]}}).residency.dirtyRecords
        const volume = (messages: Message[]) => messages.reduce((n, message) => n + message.data.length, 0)
        const overlayRows = { messages: 0, messageCharacters: 0 }
        for (const interval of overlay?.intervals(0, overlay.count) ?? []) {
            if ('messages' in interval) {
                overlayRows.messages += interval.messages.length
                overlayRows.messageCharacters += volume(interval.messages)
            }
        }
        return { coordinator: { snapshots: pending.length, messageCharacters: pending.reduce((n, p) => n + p.event.mutations.reduce((a, m) => a + volume(m.messages), 0), 0) },
            residency: { snapshots: dirty.length, messageCharacters: dirty.reduce((n, d) => n + volume(d.messages), 0) }, overlayRows }
    }
    function apply(text: string) {
        const last = chat.message.at(-1)!
        const editing = last.chatId === 'response'
        const index = editing ? chat.message.length - 1 : chat.message.length
        const next: Message = editing ? { ...last, data: text } : { role: 'char', chatId: 'response', data: text }
        const undo = overlay?.applyRange(index, editing ? [last] : [], [next], session.version + 1)
        try {
            if (editing) session.edit(session.locate(index), next)
            else session.append(next)
        } catch (error) {
            undo?.()
            throw error
        }
    }
    return { fixture, storage, database, character, chat, session, coordinator, canonicalCapture, timer, metrics, commits, projectionDurations, rebaseDurations, retention, apply,
        authority: () => authority,
        block() { let release!: () => void; blocked = new Promise<void>((done) => { release = done });
            const entered = new Promise<void>((done) => { enter = done }); return {entered, release} },
        async close() { timer.clear(); session.invalidate(); probe.store = null },
    }
}

describe('generation memory evidence using real product modules', () => {
    it('records buffered history, promotion, subsequent flush and listener costs', async () => {
        if (!['all', 'buffered'].includes(selected)) return
        for (const tokens of smoke ? [1000] : [100_000, 400_000, 1_600_000]) {
            for (const route of ['complete', 'windowed'] as const) {
                for (const metadataBytes of tokens === 1_600_000 ? [0, 1_048_576] : [0]) {
                    const run = await setup(tokens, route, metadataBytes)
                    try {
                        const preparation = route === 'complete'
                            ? await phase('history-materialization', () => run.storage.store.readConversation('character', 'conversation'))
                            : await phase('history-window-selection', async () => {
                                const lease = await run.storage.store.acquireRevision(run.coordinator.revision)
                                try { return await prepareHistoryWindow({reader: createStoreHistoryWindowReader(lease, run.authority()!), memory: 'none',
                                    conversation: run.chat, readMetadata: async () => null, preserveOrphanedMemory: false, queryChatCount: 1,
                                    tokenBudget: 32_000, countTokens: async (text) => countTokens(text), assertCurrent: () => {} }) }
                                finally { await lease.release() }
                            })
                        run.metrics.push(preparation.metric)
                        const admittedMessages = route === 'windowed' ? (preparation.value as Awaited<ReturnType<typeof prepareHistoryWindow>>).selection.messages.length : run.chat.message.length
                        run.metrics.push((await phase('apply-buffered-response', () => run.apply(response))).metric)
                        run.metrics.push((await phase('first-flush-after-promotion', () => run.coordinator.flushPendingDataLocally('benchmark-buffered'))).metric)
                        run.metrics.push((await phase('parser-complete-session-noop', () => runCurrentChatParserPass({chat: run.chat,
                            database: run.database, ownerCharacterId: 'character', parserCharacter: run.character, session: run.session,
                            parser: (data) => data}))).metric)
                        for (const listeners of [0, 1]) {
                            run.metrics.push((await phase(`full-object-listeners-${listeners}`, () => dispatchChatOutputListeners({
                                listeners: new Set(listeners ? [async (event) => { expect(event.chat.message.at(-1).data).toBe(response) }] : []),
                                char: run.character, chat: run.chat, messageIndex: run.chat.message.length - 1,
                                resolvePosition: async () => ({characterIndex: 0, chatIndex: 0}),
                                projectScalable: createProductionPluginChatOutputProjector(safeStructuredClone), onError: (error) => {throw error},
                            }))).metric)
                        }
                        run.apply(response + ' alpha')
                        run.metrics.push((await phase('subsequent-flush', () => run.coordinator.flushPendingDataLocally('benchmark-subsequent'))).metric)
                        for (let edit = 0; edit < 8; edit++) run.apply(response + ' alpha'.repeat(edit + 2))
                        run.metrics.push((await phase('eight-pending-flush', () => run.coordinator.flushPendingDataLocally('benchmark-eight-pending'))).metric)
                        const recovered = await run.storage.store.readConversation('character', 'conversation')
                        expect(recovered?.value.message.at(-1)?.data).toBe(response + ' alpha'.repeat(9))
                        expect(run.coordinator.hasPendingPersistenceWork).toBe(false)
                        expect(run.projectionDurations.length).toBeGreaterThan(0)
                        expect(run.rebaseDurations.length).toBeGreaterThan(0)
                        evidence.push({kind: 'buffered', ...run.fixture.metrics, responseTokens: countTokens(response), responseCharacters: response.length,
                            responseUtf8Bytes: Buffer.byteLength(response), generationAdmission: route === 'windowed' ? 'history-window-helper' : 'complete-materialization-helper',
                            admittedMessages, actualSaveRoute: route, phases: run.metrics, commits: run.commits, projectionMs: run.projectionDurations,
                            acknowledgementRebaseMs: run.rebaseDurations, finalRetention: run.retention(),
                            unmeasured: ['full sendChat', 'prompt preparation', 'windowed parser caller adapter', 'native transport bytes', 'renderer peak memory'],
                            caveat: 'The persistence route uses real coordinator authority. The response adapter uses a complete ActiveConversationSession; windowed controller and UI are outside this harness.'})
                    } finally { await run.close() }
                }
            }
        }
    }, 600_000)

    it('compares streamed delivery of the same 20000-token response against buffered delivery', async () => {
        if (!['all', 'delivery'].includes(selected)) return
        for (const route of ['complete', 'windowed'] as const) {
            const run = await setup(smoke ? 1000 : 1_600_000, route)
            try {
                const display = createStreamingDisplayController({mode: 'off',
                    processSemantic: async (snapshot) => run.apply(snapshot.value),
                    processPreview: async () => { throw new Error('Unexpected preview') }})
                const application = await phase('same-response-stream-application', async () => {
                    for (let i = 1; i <= 200; i++) await display.submit(response.slice(0, Math.ceil(response.length * i / 200)))
                    await display.finish()
                })
                const retained = run.retention()
                const flush = await phase('same-response-stream-final-flush', () => run.coordinator.flushPendingDataLocally('benchmark-same-response'))
                expect((await run.storage.store.readConversation('character', 'conversation'))?.value.message.at(-1)?.data).toBe(response)
                evidence.push({kind: 'same-response-stream', ...run.fixture.metrics, actualSaveRoute: route, updates: 200,
                    responseTokens: countTokens(response), responseCharacters: response.length, responseUtf8Bytes: Buffer.byteLength(response),
                    retentionBeforeFinal: retained, completion: run.retention(), phases: [application.metric, flush.metric],
                    projectionMs: run.projectionDurations, acknowledgementRebaseMs: run.rebaseDurations, commits: run.commits,
                    comparison: 'Compare the matching buffered fixture first-flush-after-promotion and apply-buffered-response phases.'})
            } finally { await run.close() }
        }
    }, 600_000)
    it('records streaming update retention with a slow commit and response checkpoint cadence', async () => {
        if (!['all', 'streaming'].includes(selected)) return
        for (const updates of smoke ? [100] : [200, 400, 800]) for (const mode of ['off', 'normal'] as const)
            for (const route of ['complete', 'windowed'] as const) for (const periodic of [false, true]) for (const blocked of periodic ? [false, true] : [false]) {
                const run = await setup(1000, route)
                const text = streamingResponse(), checkpoints: unknown[] = []
                const displayClock = clock()
                const gate = blocked ? run.block() : null
                let flight: Promise<void> | null = null
                let appliedUpdates = 0
                const display = createStreamingDisplayController({mode: mode === 'normal' ? 'balanced' : mode, clock: displayClock,
                    processSemantic: async (snapshot) => {run.apply(snapshot.value); appliedUpdates++}, processPreview: async () => {throw new Error('Unexpected preview')} })
                try {
                    const measured = await phase('streaming-updates', async () => {
                        for (let i = 1; i <= updates; i++) {
                            await display.submit(text.slice(0, Math.ceil(text.length * i / updates)))
                            await displayClock.advance(20)
                            if (periodic && i % 100 === 0) {
                                checkpoints.push({update: i, moment: 'before-request', ...run.retention()})
                                if (!flight) {
                                    flight = run.coordinator.flushPendingDataLocally('benchmark-response-checkpoint')
                                    if (gate) await gate.entered
                                    else { await flight; flight = null }
                                }
                                checkpoints.push({update: i, moment: blocked ? 'commit-blocked' : 'after-acknowledgement', ...run.retention()})
                            }
                        }
                        await display.finish()
                        checkpoints.push({update: updates, moment: 'completion-before-acknowledgement', ...run.retention()})
                        gate?.release()
                        if (flight) await flight
                        await run.coordinator.flushPendingDataLocally('benchmark-final')
                    })
                    expect((await run.storage.store.readConversation('character', 'conversation'))?.value.message.at(-1)?.data).toBe(text)
                    evidence.push({kind: 'streaming', updates, chunkIntervalMs: 20, mode, actualSaveRoute: route, periodicRequests: periodic, blockedCommit: blocked,
                        finalCharacters: text.length, finalTokens: countTokens(text), appliedUpdates, checkpoints, completion: run.retention(),
                        phases: [measured.metric], commits: run.commits, projectionMs: run.projectionDurations, acknowledgementRebaseMs: run.rebaseDurations,
                        checkpointDriver: 'Harness requests every 2000 simulated milliseconds; response-layer implementation measured separately after integration',
                        fixture: 'Equivalent-length synthetic fixture, not recovered original investigation text', rendererPeakBytes: null})
                } finally { gate?.release(); displayClock.clear(); await run.close() }
            }
    }, 600_000)
})

afterAll(async () => {
    for (const dispose of startup.disposers.splice(0)) dispose()
    JSON.stringify = originalStringify
    globalThis.structuredClone = originalClone
    delete comparisonGlobal.__generationMemoryComparison
    const directory = resolve(process.env.GENERATION_MEMORY_OUTPUT ?? '.tmp/generation-memory')
    await mkdir(directory, { recursive: true })
    const sourceHashes = Object.fromEntries(await Promise.all(files.map(async (file) => [file, createHash('sha256').update(await readFile(resolve(process.env.GENERATION_MEMORY_BASELINE_SOURCE ?? '.', file))).digest('hex')])))
    const output = resolve(directory, `${process.env.GENERATION_MEMORY_LABEL ?? 'run'}-${selected}-${withVolume ? 'volume' : 'timing'}.json`)
    await writeFile(output, originalStringify({schema: 1, timestamp: new Date().toISOString(), platform: process.platform, node: process.version,
        smoke, volumeInstrumentation: withVolume, sourceHashes, scope: 'Product modules in Vitest with fake-indexeddb. Timings are Node timings, not device memory measurements.', evidence}, null, 2))
    process.stdout.write(`GENERATION_MEMORY_ARTIFACT ${output}\n`)
})
