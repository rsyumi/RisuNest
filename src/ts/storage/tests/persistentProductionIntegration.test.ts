import 'fake-indexeddb/auto'
import { describe, expect, it, vi } from 'vitest'
import type { Database } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import {
    capturePersistentRoot,
    captureSelectedPersistentCharacter,
    createPersistentDataRuntime,
    type PersistentDataRuntimeStateAdapter,
} from '../persistentDataRuntime'
import { decodeRisuSave } from '../risuSave'
import {
    createPersistentSaveObserverInstallation,
    installPersistentSaveNotifications,
} from '../persistentSaveNotifications'

vi.mock('../database.svelte', () => ({
    getDatabase: () => {
        throw new Error('No live database in runtime integration tests')
    },
    presetTemplate: {},
}))
vi.mock('../../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

function makeDatabase(): Database {
    return {
        username: 'Fixture',
        characters: [
            {
                type: 'character',
                chaId: 'char-a',
                name: 'Alpha',
                chatPage: 0,
                chats: [
                    {
                        id: 'chat-a',
                        name: 'First',
                        message: [],
                    },
                ],
            },
        ],
    } as unknown as Database
}

function makeAdapter(database: Database): PersistentDataRuntimeStateAdapter & {
    current(): Database
} {
    let workingCopy = structuredClone(database)
    return {
        current: () => workingCopy,
        captureRoot: () => {
            const { characters: _characters, ...root } = structuredClone(workingCopy)
            return root
        },
        captureSelectedCharacter: () => structuredClone(workingCopy.characters[0] ?? null),
        captureCharacter: (id) => {
            const character = workingCopy.characters.find((item) => item.chaId === id)
            return character ? structuredClone(character) : null
        },
        getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
        replaceDatabase: (replacement) => {
            workingCopy = structuredClone(replacement)
        },
        publishCharacter: (character) => {
            const index = workingCopy.characters.findIndex((item) => item.chaId === character.chaId)
            workingCopy.characters[index] = structuredClone(character)
        },
        publishConversation: (characterId, conversation) => {
            const character = workingCopy.characters.find((item) => item.chaId === characterId)!
            const index = character.chats.findIndex((chat) => chat.id === conversation.id)
            character.chats[index] = structuredClone(conversation)
            character.chatPage = index
        },
    }
}

function makeStore(name: string) {
    return new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
}

describe('persistent production runtime', () => {
    it('captures root and the selected character without traversing inactive characters', () => {
        const database = makeDatabase()
        const inactive = structuredClone(database.characters[0])
        Object.defineProperty(inactive, 'chats', {
            enumerable: true,
            get: () => {
                throw new Error('inactive character was traversed')
            },
        })
        database.characters.push(inactive)

        expect(capturePersistentRoot(database).username).toBe('Fixture')
        expect(captureSelectedPersistentCharacter(database, 0)?.chaId).toBe('char-a')
    })

    it('commits ordinary root and selected-character edits without a legacy writer', async () => {
        const database = makeDatabase()
        const store = makeStore(`runtime-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const revisions: number[] = []
        const legacyWriter = vi.fn()
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            onLocalRevision: (revision) => revisions.push(revision),
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)

        adapter.current().username = 'Changed root'
        adapter.current().characters[0].name = 'Changed character'
        runtime.markPersistentDataDirty(64)
        await runtime.flushPendingData('test')

        const reopened = await store.materializeDatabase(runtime.revision)
        expect(reopened.username).toBe('Changed root')
        expect(reopened.characters[0].name).toBe('Changed character')
        expect(revisions).toEqual([2])
        expect(legacyWriter).not.toHaveBeenCalled()
    })

    it('commits a character addition through the production request API', async () => {
        const database = makeDatabase()
        const store = makeStore(`runtime-addition-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        const added = structuredClone(adapter.current().characters[0])
        added.chaId = 'char-added'
        added.name = 'Added character'
        added.chats[0].id = 'chat-added'
        const install = vi.fn(() => adapter.current().characters.push(added))
        adapter.current().username = 'Root with addition'
        adapter.current().characters[0].name = 'Previous selected edit'

        await runtime.commitCharacterAddition({
            characterId: added.chaId,
            estimatedBytes: 256,
            install,
        }, 'new-character')

        expect(install).toHaveBeenCalledOnce()
        const persisted = await store.materializeDatabase(runtime.revision)
        expect(persisted.username).toBe('Root with addition')
        expect(persisted.characters[0].name).toBe('Previous selected edit')
        expect(persisted.characters[1]).toEqual(added)
    })

    it('reopens an inactive character converted from a module with complete chats', async () => {
        const database = makeDatabase()
        const databaseName = `runtime-module-conversion-${crypto.randomUUID()}`
        const store = makeStore(databaseName)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        const converted = structuredClone(adapter.current().characters[0])
        converted.chaId = 'module-character'
        converted.name = 'Converted module'
        converted.chats = [{
            id: 'module-chat',
            name: 'Module chat',
            note: '',
            localLore: [],
            message: [{ role: 'char', data: 'Converted message' }],
        }]

        await runtime.commitCharacterAddition({
            characterId: converted.chaId,
            estimatedBytes: 256,
            install: () => adapter.current().characters.push(converted),
        }, 'convert-module-to-character')

        const reopened = makeStore(databaseName)
        await reopened.open()
        const persisted = await reopened.materializeDatabase(runtime.revision)
        expect(persisted.characters.find((character) => character.chaId === converted.chaId)).toEqual(
            converted,
        )
    })

    it('retries one exact dynamically selected publication and disposes it after success', async () => {
        const database = makeDatabase()
        const store = makeStore(`publisher-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const publish = vi.fn(async () => {
            if (publish.mock.calls.length === 1) throw new Error('offline')
        })
        const dispose = vi.fn(async () => undefined)
        const pin = vi.fn(async () => ({ publish, dispose }))
        let officialPublisher = { pin }
        let nowValue = 0
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            getOfficialPublisher: () => officialPublisher,
            now: () => nowValue,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        adapter.current().username = 'Published'
        runtime.markPersistentDataDirty(64)

        await expect(runtime.flushPendingData('first')).rejects.toThrow('offline')
        officialPublisher = { pin: vi.fn() }
        nowValue = 4000
        await runtime.flushPendingData('retry')

        expect(pin).toHaveBeenCalledOnce()
        expect(publish).toHaveBeenCalledTimes(2)
        expect(dispose).toHaveBeenCalledOnce()
        expect(runtime.revision).toBe(2)
    })

    it('uses the official publisher selected after local initialization', async () => {
        const database = makeDatabase()
        const store = makeStore(`late-publisher-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const publish = vi.fn(async () => undefined)
        const pin = vi.fn(async () => ({ publish, dispose: vi.fn(async () => undefined) }))
        let officialPublisher: { pin: typeof pin } | null = null
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            getOfficialPublisher: () => officialPublisher,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        }
        )
        await runtime.initializeActiveWorkingSet(database)
        officialPublisher = { pin }
        adapter.current().username = 'Account enabled'
        runtime.markPersistentDataDirty(64)

        await runtime.flushPendingData('account-enabled')

        expect(pin).toHaveBeenCalledWith(2)
        expect(publish).toHaveBeenCalledOnce()
    })

    it('uses an idempotent no-op publication when account mode remains disabled', async () => {
        const database = makeDatabase()
        const store = makeStore(`disabled-publisher-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const getOfficialPublisher = vi.fn(() => null)
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            getOfficialPublisher,
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        adapter.current().username = 'Local only'
        runtime.markPersistentDataDirty(64)

        await runtime.flushPendingData('local-only')
        await runtime.publishCurrentOfficialRevision()

        expect(getOfficialPublisher).toHaveBeenCalled()
        expect(runtime.revision).toBe(2)
    })

    it('publishes an accepted replacement revision and retains its exact handle for retry', async () => {
        const database = makeDatabase()
        const store = makeStore(`replacement-publisher-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const publish = vi.fn(async () => {
            if (publish.mock.calls.length === 1) throw new Error('official offline')
        })
        const dispose = vi.fn(async () => undefined)
        const pin = vi.fn(async () => ({ publish, dispose }))
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            officialPublisher: { pin },
            prepareDatabase: async (candidate) => structuredClone(candidate),
        })
        await runtime.initializeActiveWorkingSet(database)
        const replacement = structuredClone(database)
        replacement.username = 'Restored account backup'
        await runtime.replacePersistentDatabase(replacement, 'account-restore')

        await expect(runtime.publishCurrentOfficialRevision()).rejects.toThrow('official offline')
        await runtime.publishCurrentOfficialRevision()

        expect(pin).toHaveBeenCalledOnce()
        expect(pin).toHaveBeenCalledWith(2)
        expect(publish).toHaveBeenCalledTimes(2)
        expect(dispose).toHaveBeenCalledOnce()
    })

    it('prepares a character activation replacement before storing it', async () => {
        const database = makeDatabase()
        const store = makeStore(`runtime-activation-preparation-${crypto.randomUUID()}`)
        await store.open()
        await store.replaceFromDatabase(database)
        const adapter = makeAdapter(database)
        const prepareDatabase = vi.fn(async (candidate: Database) => ({
            ...structuredClone(candidate),
            username: 'Prepared cold replacement',
        }))
        const runtime = createPersistentDataRuntime({
            store,
            state: adapter,
            prepareDatabase,
        })
        await runtime.initializeActiveWorkingSet(database)

        expect(await runtime.activateCharacter('char-a', {
            prepare: async () => ({
                database: structuredClone(database),
                reason: 'cold-character-restore',
            }),
        })).toBe(true)

        expect(prepareDatabase).toHaveBeenCalledOnce()
        expect((await store.materializeDatabase(runtime.revision)).username).toBe(
            'Prepared cold replacement',
        )
    })

    it('broadcasts successful revisions and warns only once for foreign sessions', async () => {
        const posted: string[] = []
        let onmessage: ((event: MessageEvent) => void) | null = null
        let callbacks: {
            onLocalRevision?: (revision: number) => void
            onFlushPromise?: (promise: Promise<void> | null) => void
        } = {}
        const warning = vi.fn()
        const savingStates: boolean[] = []
        const dispose = installPersistentSaveNotifications({
            sessionId: 'local-session',
            channel: {
                postMessage: (value) => posted.push(value as string),
                close: vi.fn(),
                get onmessage() {
                    return onmessage
                },
                set onmessage(value) {
                    onmessage = value
                },
            },
            configureRuntime: (next) => {
                callbacks = next
            },
            showForeignRevisionWarning: warning,
            setSaving: (value) => savingStates.push(value),
        })

        callbacks.onLocalRevision?.(2)
        onmessage?.({ data: 'local-session' } as MessageEvent)
        onmessage?.({ data: 'foreign-a' } as MessageEvent)
        onmessage?.({ data: 'foreign-b' } as MessageEvent)
        await Promise.resolve()
        callbacks.onFlushPromise?.(Promise.resolve())
        callbacks.onFlushPromise?.(null)
        dispose()

        expect(posted).toEqual(['local-session'])
        expect(warning).toHaveBeenCalledOnce()
        expect(savingStates).toEqual([true, false])
    })

    it('stops and reinstalls the production observer without stale callbacks', () => {
        const installation = createPersistentSaveObserverInstallation()
        const warning = vi.fn()
        const savingStates: boolean[] = []
        let activeCallbacks: {
            onLocalRevision?: (revision: number) => void
            onFlushPromise?: (promise: Promise<void> | null) => void
        } = {}
        const installSession = (sessionId: string) => {
            let onmessage: ((event: MessageEvent) => void) | null = null
            const close = vi.fn()
            const disposeEffects = vi.fn()
            const channel = {
                postMessage: vi.fn(),
                close,
                get onmessage() {
                    return onmessage
                },
                set onmessage(value) {
                    onmessage = value
                },
            }
            installation.install(() => {
                const disposeNotifications = installPersistentSaveNotifications({
                    sessionId,
                    channel,
                    configureRuntime: (callbacks) => {
                        activeCallbacks = callbacks
                    },
                    showForeignRevisionWarning: warning,
                    setSaving: (value) => savingStates.push(value),
                })
                return () => {
                    disposeEffects()
                    disposeNotifications()
                }
            })
            return { channel, close, disposeEffects }
        }

        const first = installSession('first')
        const second = installSession('second')
        first.channel.onmessage?.({ data: 'foreign-old' } as MessageEvent)
        second.channel.onmessage?.({ data: 'foreign-new' } as MessageEvent)
        activeCallbacks.onFlushPromise?.(Promise.resolve())
        activeCallbacks.onFlushPromise?.(null)
        installation.stop()
        const third = installSession('third')

        expect(first.disposeEffects).toHaveBeenCalledOnce()
        expect(first.close).toHaveBeenCalledOnce()
        expect(second.disposeEffects).toHaveBeenCalledOnce()
        expect(second.close).toHaveBeenCalledOnce()
        expect(warning).toHaveBeenCalledOnce()
        expect(savingStates).toEqual([true, false])
        expect(third.close).not.toHaveBeenCalled()
        installation.stop()
        expect(third.disposeEffects).toHaveBeenCalledOnce()
        expect(third.close).toHaveBeenCalledOnce()
    })
})
