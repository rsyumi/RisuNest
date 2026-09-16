import 'fake-indexeddb/auto'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from './database.svelte'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import type {
    ContentChangeKey,
    ContentChangeWindow,
    DataRevision,
    PersistentDataStore,
} from './persistentDataStore'
import {
    capturePersistentPluginStorage,
    capturePersistentPresets,
    capturePersistentRoot,
    createPersistentDataRuntime,
    type PersistentDataRuntimeStateAdapter,
} from './persistentDataRuntime'

vi.mock('./database.svelte', () => ({
    getDatabase: () => {
        throw new Error('No global database in targeted refresh tests')
    },
    presetTemplate: {},
}))
vi.mock('../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

function makeDatabase(username: string): Database {
    return {
        username,
        botPresetsId: 0,
        characters: [
            {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chatPage: 0,
                chats: [{ id: 'chat-a', name: 'First', message: [] }],
            },
            {
                type: 'character',
                chaId: 'char-b',
                name: 'Beta',
                chatPage: 0,
                chats: [{ id: 'chat-b', name: 'Second', message: [] }],
            },
        ],
        botPresets: [{ name: 'Preset' }],
        pluginCustomStorage: {},
    } as unknown as Database
}

/// The change window every native store exposes, scripted for one refresh.
function withScriptedChangeWindow(store: IndexedDbPersistentDataStore): {
    store: PersistentDataStore
    script(window: ContentChangeWindow | null, keys: ContentChangeKey[]): void
    cursors: DataRevision[]
    queryCharacterCalls(): number
    resetCounts(): void
    failTargetedReads(failing: boolean): void
} {
    let scriptedWindow: ContentChangeWindow | null = null
    let scriptedKeys: ContentChangeKey[] = []
    const cursors: DataRevision[] = []
    let queryCharacterCalls = 0
    let failing = false
    const acquireRevision = store.acquireRevision.bind(store)
    const decorated = Object.create(store) as PersistentDataStore
    Object.assign(decorated, {
        acquireRevision: async (revision: DataRevision) => {
            const lease = await acquireRevision(revision)
            const queryCharacters = lease.queryCharacters.bind(lease)
            const readCharacterSummary = lease.readCharacterSummary.bind(lease)
            return Object.assign(Object.create(lease), {
                queryCharacters: (input: Parameters<typeof queryCharacters>[0]) => {
                    queryCharacterCalls += 1
                    return queryCharacters(input)
                },
                readCharacterSummary: async (id: string) => {
                    // Fails the targeted pass once and lets the fallback through.
                    if (failing) {
                        failing = false
                        throw new Error('Synthetic targeted read failure')
                    }
                    return readCharacterSummary(id)
                },
                readWorkingSetChangeWindow: async () =>
                    scriptedWindow ?? { revision, afterRevision: null },
                readWorkingSetChangePage: async (
                    _afterRevision: DataRevision,
                    afterKey: ContentChangeKey | null,
                ) => (afterKey === null ? scriptedKeys : []),
            })
        },
        commitWorkingSetChangeCursor: async (revision: DataRevision) => {
            cursors.push(revision)
        },
    })
    return {
        store: decorated,
        script(window, keys) {
            scriptedWindow = window
            scriptedKeys = keys
        },
        cursors,
        queryCharacterCalls: () => queryCharacterCalls,
        resetCounts() {
            queryCharacterCalls = 0
        },
        failTargetedReads(next: boolean) {
            failing = next
        },
    }
}

function makeState(database: Database): PersistentDataRuntimeStateAdapter & {
    current(): Database
    generating: { characterId: string; conversationId: string } | null
    operationActive: boolean
    endGeneration(): void
} {
    let current = structuredClone(database)
    const listeners = new Set<(active: boolean) => void>()
    const state = {
        current: () => current,
        generating: null as { characterId: string; conversationId: string } | null,
        operationActive: false,
        endGeneration() {
            state.operationActive = false
            for (const listener of listeners) listener(false)
        },
        subscribeConversationOperationActive: (listener: (active: boolean) => void) => {
            listeners.add(listener)
            listener(state.operationActive)
            return () => listeners.delete(listener)
        },
        captureRoot: () => capturePersistentRoot(current),
        capturePluginStorage: () => capturePersistentPluginStorage(current),
        capturePresets: () => capturePersistentPresets(current),
        captureSelectedCharacter: () => current.characters[0] ?? null,
        captureCharacter: (id: string) =>
            current.characters.find((character) => character.chaId === id) ?? null,
        getSelectedCharacterId: () => current.characters[0]?.chaId ?? null,
        getSelectedConversationId: () => current.characters[0]?.chats[0]?.id ?? null,
        captureWorkingSetDatabase: () => current,
        isConversationOperationActive: () => state.operationActive,
        getGeneratingConversation: () => state.generating,
        replaceDatabase: (database: Database) => {
            current = database
        },
        publishCharacter: () => undefined,
        publishConversation: () => undefined,
    }
    return state as unknown as PersistentDataRuntimeStateAdapter & {
        current(): Database
        generating: { characterId: string; conversationId: string } | null
        operationActive: boolean
        endGeneration(): void
    }
}

async function makeRuntime(name: string) {
    const raw = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
    await raw.open()
    const database = makeDatabase('Initial')
    await raw.replaceFromDatabase(database)
    const scripted = withScriptedChangeWindow(raw)
    const state = makeState(database)
    const runtime = createPersistentDataRuntime({
        store: scripted.store,
        state,
        prepareDatabase: async (candidate) => candidate,
    })
    await runtime.initializeActiveWorkingSet(database)
    return { runtime, state, store: raw, scripted }
}

async function refresh(
    runtime: Awaited<ReturnType<typeof makeRuntime>>['runtime'],
    revision: number,
): Promise<void> {
    const fence = await runtime.acquireCommittedWorkingSetRefreshFence()
    try {
        await fence.refreshCommittedWorkingSet(revision)
    } finally {
        fence.release()
    }
}

describe('the working-set refresh drives the content change cursor', () => {
    it('realigns the cursor with the initial projection of a recreated WebView', async () => {
        const { scripted } = await makeRuntime(`targeted-boot-${crypto.randomUUID()}`)
        expect(scripted.cursors).toEqual([1])
    })

    it('reprojects and advances the cursor when the window asks for a rebuild', async () => {
        const { runtime, state, store, scripted } = await makeRuntime(
            `targeted-rebuild-${crypto.randomUUID()}`,
        )
        await store.replaceFromDatabase(makeDatabase('Remote winner'), 1)
        scripted.script(null, [])
        scripted.resetCounts()
        await refresh(runtime, 2)

        expect(state.current().username).toBe('Remote winner')
        expect(scripted.queryCharacterCalls()).toBeGreaterThan(0)
        expect(scripted.cursors).toEqual([1, 2])
    })

    it('applies a bounded window without walking the character catalog', async () => {
        const { runtime, state, store, scripted } = await makeRuntime(
            `targeted-window-${crypto.randomUUID()}`,
        )
        await store.replaceFromDatabase(makeDatabase('Projected'), 1)
        scripted.script(null, [])
        await refresh(runtime, 2)

        const root = await store.readRoot()
        await store.commit({
            expectedRevision: 2,
            root: { ...root.value, username: 'Targeted' },
        })
        scripted.script({ revision: 3, afterRevision: 2 }, [
            { kind: 'root', key1: '', key2: '' },
        ])
        scripted.resetCounts()
        await refresh(runtime, 3)

        expect(state.current().username).toBe('Targeted')
        expect(scripted.queryCharacterCalls()).toBe(0)
        expect(scripted.cursors).toEqual([1, 2, 3])
    })

    it('holds the cursor while a change lands on a generating conversation', async () => {
        const { runtime, state, store, scripted } = await makeRuntime(
            `targeted-deferred-${crypto.randomUUID()}`,
        )
        await store.replaceFromDatabase(makeDatabase('Projected'), 1)
        scripted.script(null, [])
        await refresh(runtime, 2)

        await store.commit({
            expectedRevision: 2,
            conversations: [
                {
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'chat-a',
                    start: 0,
                    deleteCount: 0,
                    messages: [{ role: 'char', data: 'remote', chatId: 'remote-1' }],
                } as never,
            ],
        })
        state.operationActive = true
        state.generating = { characterId: 'char-a', conversationId: 'chat-a' }
        scripted.script({ revision: 3, afterRevision: 2 }, [
            { kind: 'character', key1: 'char-a', key2: '' },
            { kind: 'conversation', key1: 'char-a', key2: 'chat-a' },
        ])
        await refresh(runtime, 3)

        expect(scripted.cursors).toEqual([1, 2])
        const selected = state
            .current()
            .characters.find((character) => character.chaId === 'char-a')!
        expect(selected.chats[0].message).toEqual([])
    })

    it('persists the generated reply before it applies the held change', async () => {
        const { runtime, state, store, scripted } = await makeRuntime(
            `targeted-resume-${crypto.randomUUID()}`,
        )
        await store.replaceFromDatabase(makeDatabase('Projected'), 1)
        scripted.script(null, [])
        await refresh(runtime, 2)

        await store.commit({
            expectedRevision: 2,
            conversations: [
                {
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'chat-a',
                    start: 0,
                    deleteCount: 0,
                    messages: [{ role: 'char', data: 'remote', chatId: 'remote-1' }],
                } as never,
            ],
        })
        state.operationActive = true
        state.generating = { characterId: 'char-a', conversationId: 'chat-a' }
        scripted.script({ revision: 3, afterRevision: 2 }, [
            { kind: 'character', key1: 'char-a', key2: '' },
            { kind: 'conversation', key1: 'char-a', key2: 'chat-a' },
        ])
        await refresh(runtime, 3)
        expect(scripted.cursors).toEqual([1, 2])

        // The reply the generation produced is still only in the working set.
        state.current().username = 'Generated locally'
        runtime.markPersistentDataDirty(10)
        scripted.script({ revision: 4, afterRevision: 2 }, [
            { kind: 'character', key1: 'char-a', key2: '' },
            { kind: 'conversation', key1: 'char-a', key2: 'chat-a' },
        ])
        state.endGeneration()
        await vi.waitFor(() => expect(scripted.cursors.length).toBe(3))

        expect((await store.readRoot()).value.username).toBe('Generated locally')
        const selected = state
            .current()
            .characters.find((character) => character.chaId === 'char-a')!
        expect(selected.chats[0].message).toEqual([
            { role: 'char', data: 'remote', chatId: 'remote-1' },
        ])
    })

    it('reprojects and advances the cursor when a targeted pass fails', async () => {
        const { runtime, state, store, scripted } = await makeRuntime(
            `targeted-failure-${crypto.randomUUID()}`,
        )
        await store.replaceFromDatabase(makeDatabase('Projected'), 1)
        scripted.script(null, [])
        await refresh(runtime, 2)

        const root = await store.readRoot()
        await store.commit({
            expectedRevision: 2,
            root: { ...root.value, username: 'Recovered' },
        })
        scripted.script({ revision: 3, afterRevision: 2 }, [
            { kind: 'character', key1: 'char-a', key2: '' },
            { kind: 'root', key1: '', key2: '' },
        ])
        scripted.resetCounts()
        scripted.failTargetedReads(true)
        await refresh(runtime, 3)

        expect(state.current().username).toBe('Recovered')
        expect(scripted.queryCharacterCalls()).toBeGreaterThan(0)
        expect(scripted.cursors).toEqual([1, 2, 3])
    })
})
