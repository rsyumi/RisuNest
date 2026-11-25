import { IDBFactory, IDBKeyRange } from 'fake-indexeddb'
import { afterEach, describe, expect, it, vi } from 'vitest'
import { flushSync } from 'svelte'
import { get } from 'svelte/store'

vi.mock('../parser/parser.svelte', async () => (await import('../parser/parser.testSupport')).parserStubModule())
vi.mock('../process/modules', () => ({ moduleUpdate: vi.fn() }))
vi.mock('../process/scripts', () => ({ resetScriptCache: vi.fn() }))

import { characterFormatUpdate } from '../characters'
import { selectedCharID, selIdState } from '../stores.svelte'
import type { Chat, Database, Message, character } from './database.svelte'
import { getDatabase, setDatabaseLite } from './database.svelte'
import { IndexedDbPersistentDataStore } from './indexedDbPersistentDataStore'
import { CONVERSATION_RANGE_MAX_LIMIT } from './persistentDataStore'
import { createPersistentDataRuntime } from './persistentDataRuntime'
import { createProductionStateAdapter } from './persistentDataRuntime.svelte'
import { workingSetResidency } from './workingSetResidency'
import { clearStreamingConversation, setStreamingConversation } from './streamingConversationRegistry'
import { indexSideChatListRows } from '../../lib/SideBars/sideChatListRows'
import { observePersistentSaveChanges } from './persistentSaveObserver.svelte'
import { cloneConversationMetadata } from './activeConversationSession'

afterEach(() => {
    workingSetResidency.clear()
    workingSetResidency.setEvictionAllowed(true)
    selectedCharID.set(-1)
})

function makeLargeLegacyDatabase(): {
    database: Database
    messages: Message[]
} {
    // One row more than a single range read can return.
    const messages = Array.from({ length: CONVERSATION_RANGE_MAX_LIMIT + 1 }, (_, index) => ({
        role: index % 2 === 0 ? 'user' : 'char',
        data: `synthetic-message-${index}`,
        chatId: `synthetic-id-${index}`,
        time: 1_800_000_000_000 + index,
    })) as Message[]
    const conversation = {
        id: 'legacy-chat',
        name: 'Legacy metadata',
        note: 'Original note',
        message: messages,
        scriptstate: { arbitrary: ['metadata', 7] },
        lastDate: 1_800_000_010_000,
    } as unknown as Chat
    const legacyCharacter = {
        type: 'character',
        chaId: 'legacy-character',
        name: 'Legacy character',
        image: '',
        firstMessage: 'Hello',
        desc: 'Synthetic legacy fixture',
        notes: '',
        chats: [conversation],
        chatFolders: [],
        chatPage: 0,
        viewScreen: 'none',
        bias: [],
        emotionImages: [],
        globalLore: [
            {
                key: 'legacy lore',
                content: '<char> @@@end',
                activationPercent: 25,
            },
        ],
        postHistoryInstructions: 'Migrated instruction',
        replaceGlobalNote: '',
        firstMsgIndex: 7,
        lastInteraction: 1,
        newGenData: {},
        syntheticCharacterField: { preserved: true },
    } as unknown as character
    return {
        database: {
            username: 'Synthetic user',
            botPresets: [],
            plugins: [],
            characters: [legacyCharacter],
        } as unknown as Database,
        messages,
    }
}

function syntheticCharacter(id: string, history: Message[]): character {
    return {
        type: 'character',
        chaId: id,
        name: `Synthetic ${id}`,
        image: '',
        firstMessage: '',
        desc: 'Synthetic description',
        notes: '',
        chats: [{ id: `${id}-chat`, name: 'Synthetic chat', note: '', localLore: [], message: history }],
        chatFolders: [],
        chatPage: 0,
        viewScreen: 'none',
        bias: [],
        emotionImages: [],
        globalLore: [],
    } as unknown as character
}

function syntheticLibrary(type: 'character'): Database {
    const history: Message[] = [{ role: 'char', data: 'Synthetic existing history' }]
    const characters: Database['characters'] = [syntheticCharacter('second-character', structuredClone(history))]
    characters.unshift(syntheticCharacter('edited-owner', history))
    return { streamingDisplayOptimizationMode: 'balanced', characters } as unknown as Database
}

// A fresh module graph gives each boot its own production runtime, stores and coordinator.
async function bootProductionApp(indexedDB: IDBFactory, seed?: Database) {
    vi.resetModules()
    Object.assign(globalThis, { indexedDB, IDBKeyRange })
    const svelte = await import('svelte')
    const stores = await import('../stores.svelte')
    const databaseModule = await import('./database.svelte')
    const runtimeModule = await import('./persistentDataRuntime.svelte')
    const factory = await import('./persistentDataStoreFactory')
    const { bootstrapPersistentDatabase } = await import('./persistentBootstrap')
    const preparation = await import('./databasePreparation')
    const catalog = await import('./workingSetCatalog')
    const { workingSetResidency: residency } = await import('./workingSetResidency')
    const characters = await import('../characters')
    const globalApi = await import('../globalApi.svelte')
    const generation = await import('../process/generationState')
    const { createSelectedConversationOperations } = await import('../selectedConversationOperations')
    const { saveCapturedChatMessage } = await import('../chatMessageUi')
    const mutations = await import('../conversationMutations')
    const { appendDefaultChatInput } = await import('../../lib/ChatScreens/defaultChatInput')
    const { default: Harness } = await import('../../lib/SideBars/BoundCharacterEditorHarness.test.svelte')
    const chatListRows = await import('../../lib/SideBars/sideChatListRows')

    if (seed) {
        const raw = factory.getRawPersistentDataStore()
        await raw.open()
        const { database } = await preparation.prepareDatabaseForBootstrap(seed)
        await raw.replaceFromDatabase(database, 0)
    }
    // Mirrors bootstrap's local-only persistent working-set installation.
    const runtime = runtimeModule.getPersistentDataRuntime()
    const local = await bootstrapPersistentDatabase({
        store: runtime.store,
        prepareDatabase: preparation.prepareDatabaseForBootstrap,
        prepareRoot: preparation.preparePersistentRootForWorkingSet,
        projectScalableWorkingSet: (input) => catalog.projectCatalogWorkingSet(
            input.root,
            input.characters,
            catalog.createCatalogPresetWorkingSet(input.presetCatalog, input.activePreset),
        ),
    })
    runtimeModule.configurePersistentDataRuntime({
        projectWorkingSet(database, selectedCharacterId, selectedConversationId, activeCharacterIds, forceScalableProjection) {
            if (forceScalableProjection === false) return database
            const projected = catalog.isCatalogPresetWorkingSet(database.botPresets)
                ? database
                : catalog.projectCompleteScalableWorkingSet(
                    database, selectedCharacterId, runtime.revision, activeCharacterIds, selectedConversationId,
                )
            for (const character of projected.characters) {
                if (catalog.isWorkingSetCharacterStub(character)) residency.markCharacterReleased(character.chaId)
                else residency.reconcileConversationResidency(character)
            }
            return projected
        },
    })
    residency.clear()
    for (const character of local.database.characters) {
        if (catalog.isWorkingSetCharacterStub(character)) residency.markCharacterReleased(character.chaId)
    }
    databaseModule.setDatabase(local.database)
    await runtimeModule.initializeActiveWorkingSet(databaseModule.getDatabase())
    stores.selectedCharID.set(-1)
    await globalApi.saveDb()
    return {
        svelte, stores, runtime, characters, generation, mutations, residency, Harness, chatListRows,
        getDatabase: databaseModule.getDatabase,
        localSaveFailure: runtimeModule.persistentLocalSaveFailure,
        appendDefaultChatInput, saveCapturedChatMessage, createSelectedConversationOperations,
    }
}

type ProductionApp = Awaited<ReturnType<typeof bootProductionApp>>

function settleWithin<T>(promise: Promise<T>, label: string, trace: unknown[]): Promise<T> {
    let timer: ReturnType<typeof setTimeout> | undefined
    return Promise.race([
        promise,
        new Promise<never>((_, reject) => {
            timer = setTimeout(() => reject(new Error(`${label} did not settle: ${JSON.stringify(trace)}`)), 5000)
        }),
    ]).finally(() => clearTimeout(timer))
}

function selectedConversationOperations(app: ProductionApp) {
    const { runtime, stores } = app
    return app.createSelectedConversationOperations({
        captureSelectedConversationTarget: () => runtime.captureSelectedConversationTarget(),
        acquireCompleteConversation: (reason, target) => runtime.acquireCompleteConversation(reason, target),
        captureCurrent: () => {
            const character = stores.DBState.db.characters[get(stores.selectedCharID)]
            const conversation = character?.chats[character.chatPage]
            return character && conversation ? { character, conversation } : null
        },
        getCurrentSession: () => runtime.getActiveConversationSession(),
        getCurrentViewportSource: () => runtime.getActiveConversationViewportSource(),
    })
}

// DefaultChatScreen's send path up to the provider. The provider stand-in returns an
// ordinary failed result, so no response is applied or acknowledged.
async function sendUserTurnWithFailedProvider(app: ProductionApp, input: string): Promise<boolean | null> {
    const { runtime, stores, generation, mutations } = app
    if (get(generation.doingChat)) return null
    return selectedConversationOperations(app).withCompleteSelectedConversation('send-message', async (context) => {
        const requireTarget = () => {
            const authority = context.requireCurrent()
            return mutations.captureConversationMutationTarget(
                authority.character, authority.conversation, authority.session,
            )
        }
        const target = requireTarget()
        const createMessage = (data: string): Message => ({ role: 'user', data, time: Date.now(), name: null })
        if (target.character.type === 'character') {
            const appended = await app.appendDefaultChatInput({
                target,
                recaptureTarget: requireTarget,
                runInputTrigger: async () => null,
                processInput: async () => input,
                isTargetCurrent: (candidate) => {
                    context.requireCurrent()
                    const character = stores.DBState.db.characters[get(stores.selectedCharID)]
                    return mutations.isConversationMutationTargetCurrent(
                        candidate, character, character?.chats[character.chatPage], runtime.getActiveConversationSession(),
                    )
                },
                createMessage,
            })
            context.requireCurrent()
            if (!appended) return false
        } else {
            mutations.appendConversationMessage(target, createMessage(input))
        }
        await new Promise((resolve) => setTimeout(resolve, 10))
        context.requireCurrent()
        const reservation = generation.reserveGeneration()
        if (!reservation) return false
        let lease: Awaited<ReturnType<typeof runtime.acquireCompleteConversation>> | null = null
        try {
            const selection = runtime.captureSelectedConversationTarget()
            if (selection) lease = await runtime.acquireCompleteConversation('generation', selection)
            await Promise.resolve()
            return false
        } finally {
            lease?.release()
            reservation.release({ preserveBusy: false })
            generation.doingChat.set(false)
        }
    })
}

describe('windowed navigation integration', () => {
    it.each([
        ['character', 'immediate'],
        ['character', 'autosave'],
        ['character', 'failed-request'],
        ['character', 'failed-request-closed-editor'],
    ] as const)('keeps %s edits from the bound editor through %s production navigation and reboot', async (type, timing) => {
        const indexedDB = new IDBFactory()
        const previousGlobals = { indexedDB: globalThis.indexedDB, IDBKeyRange: globalThis.IDBKeyRange }
        const ownerId = 'edited-owner'
        const conversationId = `${ownerId}-chat`
        const typedName = '수정 🙂'
        const typedDescription = 'Synthetic edited description'
        const editedHistory = 'Synthetic edited history'
        const userTurn = 'Synthetic user turn'
        const trace: unknown[] = []
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
        const target = document.createElement('div')
        document.body.append(target)
        let editor: ReturnType<ProductionApp['svelte']['mount']> | null = null
        let app: ProductionApp | null = null
        const reader = new IndexedDbPersistentDataStore('risuai-persistent-data', indexedDB, IDBKeyRange)
        try {
            app = await bootProductionApp(indexedDB, syntheticLibrary(type))
            const { svelte, stores, runtime, characters } = app
            const commit = runtime.store.commit.bind(runtime.store)
            vi.spyOn(runtime.store, 'commit').mockImplementation(async (request) => {
                const entry: Record<string, unknown> = {
                    stage: 'commit',
                    expected: request.expectedRevision,
                    keys: Object.keys(request).filter((key) => key !== 'expectedRevision').sort(),
                }
                trace.push(entry)
                try {
                    const result = await commit(request)
                    entry.revision = result.revision
                    return result
                } catch (error) {
                    entry.error = error instanceof Error ? error.name : typeof error
                    throw error
                }
            })
            let dirtyMarks = 0
            const markDirty = runtime.markPersistentDataDirty.bind(runtime)
            vi.spyOn(runtime, 'markPersistentDataDirty').mockImplementation((bytes) => {
                dirtyMarks++
                markDirty(bytes)
            })
            const activate = runtime.activateCharacter.bind(runtime)
            vi.spyOn(runtime, 'activateCharacter').mockImplementation(async (id, options) => {
                const result = await activate(id, options)
                trace.push({ stage: 'activate', id, result, revision: runtime.revision, dirtyMarks })
                return result
            })
            const navigate = (id: string) => settleWithin(
                characters.changeChar(stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === id)),
                `changeChar(${id})`,
                trace,
            )
            const selected = () => stores.DBState.db.characters[get(stores.selectedCharID)]
            const readEditor = () => ({
                name: target.querySelector('input'),
                description: target.querySelector('textarea'),
            })

            editor = svelte.mount(app.Harness, { target })
            svelte.flushSync()
            expect(stores.DBState.db).toBe(app.getDatabase())
            expect(await navigate(ownerId), JSON.stringify(trace)).toBe(true)
            const inputs = await vi.waitFor(() => {
                const found = readEditor()
                expect(found.name).not.toBeNull()
                return found
            })
            const beforeEdit = dirtyMarks
            inputs.name!.value = typedName
            inputs.name!.dispatchEvent(new Event('input', { bubbles: true }))
            if (inputs.description) {
                inputs.description.value = typedDescription
                inputs.description.dispatchEvent(new Event('input', { bubbles: true }))
            }
            await svelte.tick()
            trace.push({
                stage: 'edited',
                selected: selected()?.chaId,
                nameEqual: selected()?.name === typedName,
                selectedTargetIsOwner: runtime.captureSelectedConversationTarget()?.characterId === ownerId,
                dirtyMarksSinceEdit: dirtyMarks - beforeEdit,
            })
            expect(selected()?.name).toBe(typedName)
            expect(dirtyMarks).toBeGreaterThan(beforeEdit)

            const operations = selectedConversationOperations(app)
            const acquired = await operations.acquireCompleteMessageTarget(0, 'edit-message')
            expect(acquired).not.toBeNull()
            try {
                const context = {
                    captureCurrent: () => {
                        const character = selected()
                        const conversation = character?.chats[character.chatPage]
                        return character && conversation ? { character, conversation } : null
                    },
                    getCurrentSession: () => runtime.getActiveConversationSession(),
                }
                expect(app.saveCapturedChatMessage(acquired!.target, context, editedHistory).saved).toBe(true)
            } finally {
                acquired!.release()
            }

            if (timing === 'failed-request-closed-editor') {
                await svelte.unmount(editor)
                editor = null
            }
            if (timing.startsWith('failed-request')) {
                expect(await settleWithin(sendUserTurnWithFailedProvider(app, userTurn), 'send', trace)).toBe(false)
                expect(get(app.generation.doingChat)).toBe(false)
                const session = runtime.getActiveConversationSession()
                trace.push({
                    stage: 'request-failed',
                    mode: runtime.getSelectedConversationMode(),
                    messages: session?.totalMessages,
                    version: session?.version,
                    persistedVersion: session?.persistedVersion,
                })
            }
            await reader.open()
            if (timing === 'autosave') {
                await vi.waitFor(async () => {
                    expect((await reader.readCharacter(ownerId))?.value.name).toBe(typedName)
                    expect((await reader.readConversation(ownerId, conversationId))?.value.message[0].data)
                        .toBe(editedHistory)
                }, { timeout: 3000 })
            }

            expect(await navigate('second-character'), JSON.stringify(trace)).toBe(true)
            expect(selected()?.chaId).toBe('second-character')
            expect(await navigate(ownerId), JSON.stringify(trace)).toBe(true)
            expect(selected()?.name).toBe(typedName)
            if (editor) await vi.waitFor(() => expect(readEditor().name?.value).toBe(typedName))
            const expectedMessages = [
                editedHistory,
                ...(timing.startsWith('failed-request') ? [userTurn] : []),
            ]
            if (editor) {
                const session = runtime.getActiveConversationSession()
                expect(session?.readRange(0, 8).messages.map((message) => message.data)).toEqual(expectedMessages)
            }
            // changeChar retries once after an observer invalidation; a second refusal is a lost click.
            const activations = trace.filter((entry): entry is { stage: 'activate', id: string, result: boolean } =>
                (entry as { stage?: string }).stage === 'activate')
            activations.forEach((entry, index) => {
                if (!entry.result) {
                    expect(activations[index + 1], JSON.stringify(trace)).toMatchObject({ id: entry.id, result: true })
                }
            })
            expect(consoleError.mock.calls.map((args) => args.map((value) =>
                value instanceof Error ? `${value.name}: ${value.message}` : String(value).slice(0, 200))),
            JSON.stringify(trace)).toEqual([])

            const persistedCharacter = await reader.readCharacter(ownerId)
            expect(persistedCharacter?.value.name).toBe(typedName)
            if (type === 'character') expect(persistedCharacter?.value).toMatchObject({ desc: typedDescription })
            expect((await reader.readConversation(ownerId, conversationId))?.value.message.map((message) => message.data))
                .toEqual(expectedMessages)
            expect((await reader.readRoot()).revision).toBe(runtime.revision)
            if (editor) await svelte.unmount(editor)
            editor = null

            // Module reset cannot stop this instance's debounce timer as a process exit would,
            // so run the production exit drain before another instance opens the same store.
            await runtime.flushPendingData('exit')
            expect((await reader.readRoot()).revision).toBe(runtime.revision)
            const rebooted = await bootProductionApp(indexedDB)
            app = rebooted
            expect(await settleWithin(rebooted.characters.changeChar(
                rebooted.stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === ownerId),
            ), 'reboot changeChar', trace)).toBe(true)
            const reopened = rebooted.stores.DBState.db.characters[get(rebooted.stores.selectedCharID)]
            expect(reopened?.name).toBe(typedName)
            const lease = await rebooted.runtime.acquireCompleteConversation('reboot-check')
            try {
                expect(lease.session.readRange(0, 8).messages.map((message) => message.data)).toEqual(expectedMessages)
            } finally {
                lease.release()
            }
            await rebooted.runtime.flushPendingData('exit')
            expect(consoleError.mock.calls.length, JSON.stringify(trace)).toBe(0)
        } finally {
            if (editor && app) await app.svelte.unmount(editor)
            target.remove()
            consoleError.mockRestore()
            Object.assign(globalThis, previousGlobals)
        }
    })

    it.each(['immediate', 'autosave'])('preserves cold-selection editor changes and a user-only turn through %s navigation', async (timing) => {
        const indexedDB = new IDBFactory()
        const databaseName = `cold-selection-${timing}`
        const store = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await store.open()
        const { database } = makeLargeLegacyDatabase()
        const first = database.characters[0] as character
        first.chats[0].message = [{ role: 'char', data: 'Synthetic existing history' }]
        const second = structuredClone(first)
        second.chaId = 'second-character'
        second.chats[0].id = 'second-chat'
        database.characters.push(second)
        await store.replaceFromDatabase(database)
        setDatabaseLite(structuredClone(database))
        selectedCharID.set(-1)
        const state = createProductionStateAdapter()
        expect(state.canonicalCapture!.character()).toBeNull()
        const runtime = createPersistentDataRuntime({ store, state, prepareDatabase: async (value) => value })
        await runtime.initializeActiveWorkingSet(getDatabase())
        const navigate = async (id: string) => {
            const expectedGeneration = runtime.getNavigationGeneration() + 1
            if (await runtime.activateCharacter(id)) return true
            if (runtime.getNavigationGeneration() !== expectedGeneration) return false
            // Match changeChar's single retry after observer invalidation.
            return runtime.activateCharacter(id)
        }
        const dispose = observePersistentSaveChanges({
            readDatabase: getDatabase,
            readSelectedCharacter: () => getDatabase().characters[selIdState.selId] ?? null,
            markDirty: (bytes) => runtime.markPersistentDataDirty(bytes),
        })
        try {
            flushSync()
            expect(await navigate(first.chaId)).toBe(true)
            const lease = await runtime.acquireCompleteConversation('synthetic-bound-editor')
            try {
                const live = getDatabase().characters[selIdState.selId] as character
                live.name = '수정 🙂'
                live.desc = 'Synthetic edited description'
                live.chats[0].note = 'Synthetic edited note'
                lease.session.append({ role: 'user', data: 'Synthetic user-only turn' })
                flushSync()
                if (timing === 'autosave') {
                    await vi.waitFor(async () => {
                        expect((await store.readCharacter(first.chaId))?.value.name).toBe('수정 🙂')
                        expect((await store.readConversation(first.chaId, 'legacy-chat'))?.value.message).toHaveLength(2)
                    }, { timeout: 3000 })
                }
            } finally {
                lease.release()
            }
            expect(await navigate(second.chaId)).toBe(true)
            expect(await navigate(first.chaId)).toBe(true)
            expect(getDatabase().characters[selIdState.selId].name).toBe('수정 🙂')
            const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
            await reopened.open()
            expect((await reopened.readCharacter(first.chaId))?.value).toMatchObject({
                name: '수정 🙂', desc: 'Synthetic edited description',
            })
            expect((await reopened.readConversation(first.chaId, 'legacy-chat'))?.value).toMatchObject({
                note: 'Synthetic edited note',
                message: [
                    { role: 'char', data: 'Synthetic existing history' },
                    { role: 'user', data: 'Synthetic user-only turn' },
                ],
            })
        } finally {
            dispose()
        }
    })

    it.each<{folders: character['chatFolders'] | undefined}>([{folders: undefined}, {folders: []}, {folders: [{id: 'folder', name: 'Folder', folded: false}]}])(
        'projects ordinary empty conversation folders into the windowed compatibility view ($folders)', async ({folders}) => {
            const database = syntheticLibrary('character')
            const owner = database.characters[0]
            if (folders === undefined) delete (owner as Partial<character>).chatFolders
            else owner.chatFolders = folders
            const store = new IndexedDbPersistentDataStore(`received-folder-view-${JSON.stringify(folders)}`, new IDBFactory(), IDBKeyRange)
            await store.open()
            const {revision} = await store.replaceFromDatabase(database)
            setDatabaseLite(structuredClone(database))
            selectedCharID.set(-1)
            workingSetResidency.setEvictionAllowed(true)
            const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value})
            await runtime.initializeActiveWorkingSet(getDatabase())
            const commit = vi.spyOn(store, 'commit')
            expect(await runtime.activateCharacter(owner.chaId)).toBe(true)
            expect(runtime.getSelectedConversationMode()).toBe('windowed')
            const live = getDatabase().characters[0]
            expect(live.chatFolders).toEqual(folders ?? [])
            expect(indexSideChatListRows(live.chats, live.chatFolders).folders).toHaveLength(folders?.length ?? 0)
            await runtime.flushPendingDataLocally('received-folder-view-no-op')
            expect(commit).not.toHaveBeenCalled()
            expect((await store.readRoot()).revision).toBe(revision)
            const stored = (await store.readCharacter(owner.chaId))!.value
            if (folders === undefined) expect(Object.hasOwn(stored, 'chatFolders')).toBe(false)
            else expect(stored.chatFolders).toEqual(folders)
            const target = runtime.captureSelectedConversationTarget()!
            const added = {id: 'added-folder', name: 'Added folder', folded: false}
            expect(runtime.editWindowedChatList(target, (candidate) => {
                candidate.chatFolders.push(added)
                return null
            }).kind).toBe('applied')
            await runtime.flushPendingDataLocally('explicit-folder-edit')
            expect(commit).toHaveBeenCalledOnce()
            expect((await store.readCharacter(owner.chaId))!.value.chatFolders).toEqual([...(folders ?? []), added])
        },
    )

    it.each([null, {invalid: true}])('rejects a non-array folder projection without defaulting it (%j)', async (folders) => {
        const database = syntheticLibrary('character')
        database.characters[0].chatFolders = folders as unknown as character['chatFolders']
        const store = new IndexedDbPersistentDataStore(`invalid-folder-view-${JSON.stringify(folders)}`, new IDBFactory(), IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(database)
        setDatabaseLite(structuredClone(database))
        selectedCharID.set(-1)
        workingSetResidency.setEvictionAllowed(true)
        const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value})
        await runtime.initializeActiveWorkingSet(getDatabase())
        const commit = vi.spyOn(store, 'commit')
        await expect(runtime.activateCharacter(database.characters[0].chaId)).rejects.toThrow('returned invalid conversation folders')
        expect(commit).not.toHaveBeenCalled()
        expect((await store.readCharacter(database.characters[0].chaId))!.value.chatFolders).toEqual(folders)
    })

    it.each([false, true])('takes the streaming state of a saved conversation from running generations only (running: %s)', async (running) => {
        // A checkpoint saves the streaming flag, but a later document has no generation behind it.
        const database = syntheticLibrary('character')
        const owner = database.characters[0]
        const conversationId = owner.chats[0].id!
        Object.assign(owner.chats[0], { isStreaming: true, activeStreamingDisplayOptimizationMode: 'balanced' })
        const store = new IndexedDbPersistentDataStore(`recovered-streaming-${running}`, new IDBFactory(), IDBKeyRange)
        await store.open()
        await store.replaceFromDatabase(database)
        const live = structuredClone(database)
        delete live.characters[0].chats[0].isStreaming
        delete live.characters[0].chats[0].activeStreamingDisplayOptimizationMode
        setDatabaseLite(live)
        selectedCharID.set(-1)
        workingSetResidency.setEvictionAllowed(true)
        if (running) setStreamingConversation(conversationId, 'balanced')
        try {
            const runtime = createPersistentDataRuntime({store, state: createProductionStateAdapter(), prepareDatabase: async (value) => value})
            await runtime.initializeActiveWorkingSet(getDatabase())
            const expectStreaming = () => {
                const chat = getDatabase().characters[0].chats[0]
                expect(chat.isStreaming ?? false).toBe(running)
                expect(chat.activeStreamingDisplayOptimizationMode).toBe(running ? 'balanced' : undefined)
            }
            expect(await runtime.activateCharacter(owner.chaId)).toBe(true)
            expect(runtime.getSelectedConversationMode()).toBe('windowed')
            expectStreaming()
            const lease = await runtime.acquireCompleteConversation('recovered-streaming', runtime.captureSelectedConversationTarget())
            try {
                expect(runtime.getSelectedConversationMode()).toBe('complete')
                expectStreaming()
            } finally {
                lease.release()
            }
        } finally {
            clearStreamingConversation(conversationId)
        }
    })

    it('normalizes through the production Svelte adapter and survives immediate promotion and restart', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'windowed-navigation-normalize-promote-restart'
        const store = new IndexedDbPersistentDataStore(
            databaseName,
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const { database, messages } = makeLargeLegacyDatabase()
        const imported = await store.replaceFromDatabase(database)

        setDatabaseLite(structuredClone(database))
        selectedCharID.set(0)
        workingSetResidency.setEvictionAllowed(false)
        const runtime = createPersistentDataRuntime({
            store,
            state: createProductionStateAdapter(),
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(getDatabase())
        workingSetResidency.setEvictionAllowed(true)

        const readConversation = vi.spyOn(store, 'readConversation')
        expect(
            await runtime.activateCharacter('legacy-character', {
                normalize: (candidate) =>
                    characterFormatUpdate(candidate, {
                        updateInteraction: true,
                    }),
            }),
        ).toBe(true)

        expect(runtime.getSelectedConversationMode()).toBe('windowed')
        expect(readConversation).not.toHaveBeenCalled()
        const windowedCharacter = getDatabase().characters[0] as character
        const windowedConversation = windowedCharacter.chats[0]
        expect(windowedCharacter).toMatchObject({
            chaId: 'legacy-character',
            postHistoryInstructions: null,
            firstMsgIndex: 7,
            syntheticCharacterField: { preserved: true },
        })
        expect(windowedCharacter.lastInteraction).toBeGreaterThan(1)
        expect(windowedCharacter.globalLore[0]).toMatchObject({
            bookVersion: 2,
            activationPercent: null,
            content: '@@probability 25\n{{char}} @@depth 0',
        })
        expect(windowedConversation).toMatchObject({
            id: 'legacy-chat',
            note: 'Original note\nMigrated instruction',
            fmIndex: 7,
            localLore: [],
            scriptstate: { arbitrary: ['metadata', 7] },
        })

        const target = runtime.captureSelectedConversationTarget()
        expect(target).not.toBeNull()
        const completeLease = await runtime.acquireCompleteConversation(
            'integration-immediate-promotion',
            target,
        )
        try {
            expect(runtime.getSelectedConversationMode()).toBe('complete')
            expect(readConversation).toHaveBeenCalledTimes(1)
            expect(completeLease.session.totalMessages).toBe(messages.length)
            expect(completeLease.session.readRange(0, 128).messages).toEqual(
                messages.slice(0, 128),
            )
            expect(completeLease.session.readLatest(128).messages).toEqual(
                messages.slice(-128),
            )

            const reopened = new IndexedDbPersistentDataStore(
                databaseName,
                indexedDB,
                IDBKeyRange,
            )
            await reopened.open()
            const detail = await reopened.readCharacter('legacy-character')
            const persisted = await reopened.readConversation(
                'legacy-character',
                'legacy-chat',
            )
            expect(detail).toMatchObject({
                revision: imported.revision + 1,
                value: {
                    postHistoryInstructions: null,
                    firstMsgIndex: 7,
                    syntheticCharacterField: { preserved: true },
                },
            })
            expect(persisted).toMatchObject({
                revision: imported.revision + 1,
                value: {
                    note: 'Original note\nMigrated instruction',
                    fmIndex: 7,
                    localLore: [],
                    scriptstate: { arbitrary: ['metadata', 7] },
                },
            })
            expect(persisted?.value.message).toEqual(messages)
            expect(
                persisted?.value.note.match(/Migrated instruction/g),
            ).toHaveLength(1)
        } finally {
            completeLease.release()
        }
    })

    // Server sync pulls when a conversation opens and applies the page under paused writes,
    // so the receive reaches the coordinator queue while the bound editor promotes the chat.
    it.each([
        ['a sync receive is queued', async (app: ProductionApp) => {
            await app.runtime.withPausedPersistentWrites('lww-receive', () => new Promise((resolve) => setTimeout(resolve, 20)))
        }],
        ['a save moves the revision', async (app: ProductionApp) => {
            app.stores.DBState.db.username = 'Synthetic renamed user'
            app.runtime.markPersistentDataDirty(1)
        }],
    ] as const)('loads the bound editor for the next character when %s during its promotion', async (_name, interrupt) => {
        const indexedDB = new IDBFactory()
        const previousGlobals = { indexedDB: globalThis.indexedDB, IDBKeyRange: globalThis.IDBKeyRange }
        const target = document.createElement('div')
        document.body.append(target)
        let editor: ReturnType<ProductionApp['svelte']['mount']> | null = null
        let app: ProductionApp | null = null
        const trace: unknown[] = []
        try {
            app = await bootProductionApp(indexedDB, syntheticLibrary('character'))
            const { svelte, stores, runtime, characters } = app
            const navigate = (id: string) => settleWithin(
                characters.changeChar(stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === id)),
                `changeChar(${id})`,
                trace,
            )
            editor = svelte.mount(app.Harness, { target })
            svelte.flushSync()
            expect(await navigate('edited-owner')).toBe(true)
            await vi.waitFor(() => expect(target.querySelector('input')?.value).toBe('Synthetic edited-owner'))

            const acquire = runtime.acquireCompleteConversation.bind(runtime)
            vi.spyOn(runtime, 'acquireCompleteConversation').mockImplementation(async (reason, selection) => {
                try {
                    return await acquire(reason, selection)
                } catch (error) {
                    trace.push({ stage: 'acquire-failed', reason, error: error instanceof Error ? error.message : String(error) })
                    throw error
                }
            })
            const read = runtime.store.readConversation.bind(runtime.store)
            let interrupted: Promise<void> | null = null
            vi.spyOn(runtime.store, 'readConversation').mockImplementation(async (characterId, conversationId) => {
                const value = await read(characterId, conversationId)
                if (characterId === 'second-character' && !interrupted) interrupted = interrupt(app!)
                return value
            })
            expect(await navigate('second-character')).toBe(true)
            await vi.waitFor(() => expect(target.querySelector('input')?.value).toBe('Synthetic second-character'))
            await interrupted
            expect(target.querySelector('[role="alert"]'), JSON.stringify(trace)).toBeNull()
            expect(trace).toEqual([])
            expect(runtime.captureSelectedConversationTarget()?.characterId).toBe('second-character')
            expect(runtime.getSelectedConversationMode()).toBe('complete')
        } finally {
            if (editor && app) await app.svelte.unmount(editor)
            target.remove()
            Object.assign(globalThis, previousGlobals)
        }
    })

    // A tap on another character fences the open promotion seconds before that character
    // publishes on a phone. A fence can also end without moving the selection.
    it.each([
        ['another character is selected', 'edited-owner'],
        ['a navigation fence keeps the selection', 'second-character'],
    ] as const)('never shows the bound editor failure when %s during its promotion', async (_name, expected) => {
        const indexedDB = new IDBFactory()
        const previousGlobals = { indexedDB: globalThis.indexedDB, IDBKeyRange: globalThis.IDBKeyRange }
        const target = document.createElement('div')
        document.body.append(target)
        let editor: ReturnType<ProductionApp['svelte']['mount']> | null = null
        let app: ProductionApp | null = null
        const trace: unknown[] = []
        const painted: string[] = []
        const collect = (records: MutationRecord[]) => {
            for (const record of records) {
                for (const node of record.addedNodes) {
                    if (!(node instanceof Element)) continue
                    const alert = node.matches('[role="alert"]') ? node : node.querySelector('[role="alert"]')
                    if (alert) painted.push(alert.textContent ?? '')
                }
            }
        }
        const observer = new MutationObserver(collect)
        try {
            app = await bootProductionApp(indexedDB, syntheticLibrary('character'))
            const { svelte, stores, runtime, characters } = app
            const navigate = (id: string) => settleWithin(
                characters.changeChar(stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === id)),
                `changeChar(${id})`,
                trace,
            )
            editor = svelte.mount(app.Harness, { target })
            svelte.flushSync()
            expect(await navigate('edited-owner')).toBe(true)
            await vi.waitFor(() => expect(target.querySelector('input')?.value).toBe('Synthetic edited-owner'))
            observer.observe(target, { childList: true, subtree: true })

            let refusalSeen!: () => void
            const refused = new Promise<void>((resolve) => { refusalSeen = resolve })
            const acquire = runtime.acquireCompleteConversation.bind(runtime)
            vi.spyOn(runtime, 'acquireCompleteConversation').mockImplementation(async (reason, selection) => {
                try {
                    return await acquire(reason, selection)
                } catch (error) {
                    const name = error instanceof Error ? error.name : String(error)
                    trace.push({ stage: 'acquire-failed', reason, characterId: selection?.characterId, error: name })
                    refusalSeen()
                    throw error
                }
            })
            let interrupted = false
            let overlapping: Promise<boolean> | null = null
            const laterReads: string[] = []
            const readConversation = runtime.store.readConversation.bind(runtime.store)
            vi.spyOn(runtime.store, 'readConversation').mockImplementation(async (characterId, conversationId) => {
                if (interrupted) laterReads.push(characterId)
                else if (characterId === 'second-character') {
                    interrupted = true
                    if (expected === 'edited-owner') overlapping = navigate('edited-owner')
                    else runtime.fenceNavigation()
                }
                return readConversation(characterId, conversationId)
            })
            // The next character publishes only after the refusal reached the editor, as on the phone.
            const readCharacter = runtime.store.readCharacter.bind(runtime.store)
            vi.spyOn(runtime.store, 'readCharacter').mockImplementation(async (id) => {
                if (interrupted && id === 'edited-owner') await refused
                return readCharacter(id)
            })
            await navigate('second-character')
            await vi.waitFor(() => expect(interrupted).toBe(true))
            if (expected === 'edited-owner') expect(await overlapping).toBe(true)
            await vi.waitFor(() => expect(target.querySelector('input')?.value).toBe(`Synthetic ${expected}`))
            collect(observer.takeRecords())
            expect(painted, JSON.stringify(trace)).toEqual([])
            expect(trace).toContainEqual({
                stage: 'acquire-failed',
                reason: 'bound-editor',
                characterId: 'second-character',
                error: 'SelectedConversationPromotionStaleError',
            })
            // The superseded promotion is not started again while the next character loads.
            if (expected === 'edited-owner') expect(laterReads).not.toContain('second-character')
            expect(runtime.captureSelectedConversationTarget()?.characterId).toBe(expected)
            expect(runtime.getSelectedConversationMode()).toBe('complete')
        } finally {
            observer.disconnect()
            if (editor && app) await app.svelte.unmount(editor)
            target.remove()
            Object.assign(globalThis, previousGlobals)
        }
    })
})

describe('selected chat list edits', () => {
    it.each(['character'] as const)('adds, duplicates, reorders and removes %s chats beside the bound editor', async (type) => {
        const indexedDB = new IDBFactory()
        const previousGlobals = { indexedDB: globalThis.indexedDB, IDBKeyRange: globalThis.IDBKeyRange }
        const ownerId = 'edited-owner'
        const originalId = `${ownerId}-chat`
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
        const target = document.createElement('div')
        document.body.append(target)
        let editor: ReturnType<ProductionApp['svelte']['mount']> | null = null
        let app: ProductionApp | null = null
        const reader = new IndexedDbPersistentDataStore('risuai-persistent-data', indexedDB, IDBKeyRange)
        try {
            app = await bootProductionApp(indexedDB, syntheticLibrary(type))
            const { svelte, stores, runtime, characters, chatListRows } = app
            const selected = () => stores.DBState.db.characters[get(stores.selectedCharID)]
            const selectedChatId = () => selected().chats[selected().chatPage]?.id
            const chatIds = () => selected().chats.map((chat) => chat.id)
            editor = svelte.mount(app.Harness, { target })
            svelte.flushSync()
            expect(await characters.changeChar(
                stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === ownerId),
            )).toBe(true)
            await vi.waitFor(() => expect(target.querySelector('input')).not.toBeNull())

            // Consecutive clicks land while the editor is still reacquiring the new selection.
            expect(await characters.addNewChat(selected())).toBe(true)
            const first = chatIds()[0]
            expect(selectedChatId()).toBe(first)
            expect(await characters.addNewChat(selected())).toBe(true)
            const second = chatIds()[0]
            expect(selectedChatId()).toBe(second)
            expect(chatIds()).toEqual([second, first, originalId])

            expect(await characters.duplicateChat(ownerId, originalId)).toBe(true)
            const duplicate = chatIds()[0]
            expect(selectedChatId()).toBe(duplicate)
            expect(selected().chats[0].name).toBe('Synthetic chat (Copy)')

            const reversed = [...chatIds()].reverse()
            expect(await characters.editSelectedChatList(ownerId, 'reorder-chats', (character) => {
                const chats = chatListRows.orderChatsByDroppedRows(
                    character.chats,
                    reversed.map((id) => ({ id, folderId: null })),
                )
                if (!chats) return false
                character.chats = chats
                return null
            })).toBe(true)
            expect(chatIds()).toEqual(reversed)
            expect(selectedChatId()).toBe(duplicate)

            expect(await characters.removeChat(selected(), duplicate)).toBe(true)
            expect(chatIds()).not.toContain(duplicate)
            expect(selectedChatId()).toBe(chatIds()[0])
            expect(await characters.removeChat(selected(), second)).toBe(true)
            expect(chatIds()).toEqual([originalId, first])
            expect(selectedChatId()).toBe(originalId)

            expect(runtime.pendingWorkingSetRefreshRevision).toBeNull()
            expect(consoleError.mock.calls.map((args) => args.map((value) =>
                value instanceof Error ? `${value.name}: ${value.message}` : String(value).slice(0, 200)))).toEqual([])

            await svelte.unmount(editor)
            editor = null
            await runtime.flushPendingData('exit')
            await reader.open()
            expect((await reader.readRoot()).revision).toBe(runtime.revision)
            const created = await reader.readConversation(ownerId, first)
            expect(created?.value.name).toBe('New Chat 2')
            expect(created?.value.message.map((message) => message.saying ?? null))
                .toEqual([])
            expect(await reader.readConversation(ownerId, duplicate)).toBeNull()
            expect(await reader.readConversation(ownerId, second)).toBeNull()

            const rebooted = await bootProductionApp(indexedDB)
            app = rebooted
            expect(await rebooted.characters.changeChar(
                rebooted.stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === ownerId),
            )).toBe(true)
            const reopened = rebooted.stores.DBState.db.characters[get(rebooted.stores.selectedCharID)]
            expect(reopened.chats.map((chat) => chat.id)).toEqual([originalId, first])
            expect(reopened.chats[reopened.chatPage].id).toBe(originalId)
            await rebooted.runtime.flushPendingData('exit')
        } finally {
            if (editor && app) await app.svelte.unmount(editor)
            target.remove()
            consoleError.mockRestore()
            Object.assign(globalThis, previousGlobals)
        }
    })

    it.each(['character'] as const)('edits the %s list around a windowed selection without loading the selected conversation', async (type) => {
        const indexedDB = new IDBFactory()
        const previousGlobals = { indexedDB: globalThis.indexedDB, IDBKeyRange: globalThis.IDBKeyRange }
        const ownerId = 'edited-owner'
        const selectedId = `${ownerId}-chat`
        const otherId = `${ownerId}-other`
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
        const reader = new IndexedDbPersistentDataStore('risuai-persistent-data', indexedDB, IDBKeyRange)
        const history = Array.from({ length: 300 }, (_, index) => ({
            role: index % 2 === 0 ? 'user' : 'char',
            data: `Synthetic windowed message ${index}`,
            chatId: `synthetic-windowed-${index}`,
        })) as Message[]
        const library = syntheticLibrary(type)
        const owner = library.characters[0] as character
        owner.chats[0].message = history
        owner.chats.push({
            id: otherId,
            name: 'Other chat',
            note: 'Other note',
            localLore: [{ key: 'other', comment: 'Other lore', content: 'kept', mode: 'normal', insertorder: 100, alwaysActive: false, secondkey: '', selective: false }],
            message: [{ role: 'user', data: 'Other history' }],
        } as unknown as Chat)
        owner.chatFolders = [{ id: 'folder-a', name: 'Folder A', folded: false }] as character['chatFolders']
        let app: ProductionApp | null = null
        try {
            app = await bootProductionApp(indexedDB, library)
            const { stores, runtime, characters, chatListRows } = app
            const selected = () => stores.DBState.db.characters[get(stores.selectedCharID)] as character
            const chatIds = () => selected().chats.map((chat) => chat.id)
            expect(await characters.changeChar(
                stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === ownerId),
            )).toBe(true)
            expect(runtime.getSelectedConversationMode()).toBe('windowed')
            expect(selected().chats[selected().chatPage].id).toBe(selectedId)
            // Activation changes are saved separately from the list edits below.
            await runtime.flushPendingData('activation')

            const readConversation = vi.spyOn(runtime.store, 'readConversation')
            const committed: string[][] = []
            const commit = runtime.store.commit.bind(runtime.store)
            vi.spyOn(runtime.store, 'commit').mockImplementation(async (request) => {
                committed.push((request.conversations ?? []).map((mutation) =>
                    `${mutation.type}:${mutation.type === 'reorder' ? mutation.conversationIds.join(',') : mutation.conversationId}`))
                return commit(request)
            })

            // Moving the other chat into a folder only changes its folder on disk.
            expect(await characters.editSelectedChatList(ownerId, 'reorder-chats', (character) => {
                const chats = chatListRows.orderChatsByDroppedRows(character.chats, [
                    { id: otherId, folderId: 'folder-a' },
                    { id: selectedId, folderId: null },
                ])
                if (!chats) return false
                character.chats = chats
                return null
            })).toBe(true)
            expect(chatIds()).toEqual([otherId, selectedId])
            expect(selected().chatPage).toBe(1)

            const imported: Chat = {
                id: `${ownerId}-imported`,
                name: 'Imported chat',
                note: 'Imported note',
                localLore: [],
                message: [{ role: 'user', data: 'Imported history' }],
            }
            expect(await characters.editSelectedChatList(ownerId, 'import-chat', (character) => {
                character.chats.unshift(imported)
                return null
            })).toBe(true)
            expect(chatIds()).toEqual([imported.id, otherId, selectedId])

            expect(await characters.removeChat(selected(), otherId)).toBe(true)
            expect(chatIds()).toEqual([imported.id, selectedId])
            expect(selected().chats[selected().chatPage].id).toBe(selectedId)
            expect(runtime.getSelectedConversationMode()).toBe('windowed')

            expect(await characters.addNewChat(selected())).toBe(true)
            const added = chatIds()[0]
            expect(chatIds()).toEqual([added, imported.id, selectedId])
            expect(selected().chats[selected().chatPage].id).toBe(added)

            expect(readConversation.mock.calls.filter(([, id]) => id === selectedId)).toEqual([])
            expect(committed.slice(0, 4)).toEqual([
                [`replace-range:${otherId}`, `reorder:${otherId},${selectedId}`],
                [`replace-range:${imported.id}`, `reorder:${imported.id},${otherId},${selectedId}`],
                [`delete:${otherId}`],
                [`replace-range:${added}`, `reorder:${added},${imported.id},${selectedId}`],
            ])
            expect(runtime.pendingWorkingSetRefreshRevision).toBeNull()
            expect(consoleError.mock.calls.map((args) => args.map((value) =>
                value instanceof Error ? `${value.name}: ${value.message}` : String(value).slice(0, 200)))).toEqual([])

            await runtime.flushPendingData('exit')
            await reader.open()
            expect((await reader.readRoot()).revision).toBe(runtime.revision)
            const order = await reader.queryConversations({ characterId: ownerId, order: 'configured', limit: 10 })
            expect(order.items.map((item) => item.id)).toEqual([added, imported.id, selectedId])
            expect((await reader.readConversation(ownerId, selectedId))?.value.message).toEqual(history)
            expect((await reader.readConversation(ownerId, imported.id!))?.value).toMatchObject({
                note: 'Imported note',
                message: imported.message,
            })
            expect(await reader.readConversation(ownerId, otherId)).toBeNull()
            expect((await reader.readConversation(ownerId, added))?.value.message
                .map((message) => message.saying ?? null))
                .toEqual([])
        } finally {
            consoleError.mockRestore()
            Object.assign(globalThis, previousGlobals)
        }
    })

    // Opening a character leaves its activation edits unsaved until the save debounce runs, and a
    // streamed reply can land while the addition is being stored.
    it.each([
        { route: 'create', unsaved: 'nothing' },
        { route: 'import', unsaved: 'nothing' },
        { route: 'create', unsaved: 'its activation' },
        { route: 'import', unsaved: 'its activation' },
        { route: 'create', unsaved: 'a reply streamed during the addition' },
    ] as const)('adds a character by $route while a windowed conversation is open with $unsaved unsaved and keeps saving and navigating', async ({ route, unsaved }) => {
        const indexedDB = new IDBFactory()
        const previousGlobals = { indexedDB: globalThis.indexedDB, IDBKeyRange: globalThis.IDBKeyRange }
        const ownerId = 'edited-owner'
        const selectedId = `${ownerId}-chat`
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
        const reader = new IndexedDbPersistentDataStore('risuai-persistent-data', indexedDB, IDBKeyRange)
        const history = Array.from({ length: 300 }, (_, index) => ({
            role: index % 2 === 0 ? 'user' : 'char',
            data: `Synthetic windowed message ${index}`,
            chatId: `synthetic-windowed-${index}`,
        })) as Message[]
        const library = syntheticLibrary('character')
        library.characters[0].chats[0].message = history
        const importedHistory: Message[] = [{ role: 'char', data: 'Synthetic imported greeting' }]
        try {
            const app = await bootProductionApp(indexedDB, library)
            const { stores, runtime, characters } = app
            const selected = () => stores.DBState.db.characters[get(stores.selectedCharID)] as character
            const indexOf = (id: string) => stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === id)
            expect(await characters.changeChar(indexOf(ownerId))).toBe(true)
            expect(runtime.getSelectedConversationMode()).toBe('windowed')
            if (unsaved !== 'its activation') await runtime.flushPendingData('activation')
            const reply = { role: 'char', data: 'Synthetic streamed reply' } as Message
            const streamReply = () => {
                const target = runtime.captureSelectedConversationTarget()!
                const authority = runtime.captureSelectedConversationAuthority()!
                const owner = stores.DBState.db.characters[indexOf(ownerId)]
                const chat = { ...cloneConversationMetadata(owner.chats.find((value) => value.id === selectedId)!), message: [] } as unknown as Chat
                const controller = runtime.captureWindowedConversationMutationController(target, chat, authority.totalMessages)!
                expect(controller.applyRange(0, 0, [reply], 'append')).toBe(true)
                controller.release()
            }
            await reader.open()
            const readConversation = vi.spyOn(runtime.store, 'readConversation')
            const committed: { keys: string[], units: string[], added?: string, replaced?: string }[] = []
            const historiesAfterCommit: Message[][] = []
            const commit = runtime.store.commit.bind(runtime.store)
            // The row the conversation shows for the reply when the save after the addition starts.
            let replyRowBeforeItsSave: Message | null | undefined
            vi.spyOn(runtime.store, 'commit').mockImplementation(async (request) => {
                if (unsaved === 'a reply streamed during the addition' && replyRowBeforeItsSave === undefined && committed.some((entry) => entry.added)) {
                    replyRowBeforeItsSave = runtime.getActiveConversationViewportSource()!.snapshot().rowAt(history.length)?.message ?? null
                }
                committed.push({
                    keys: Object.keys(request).filter((key) => key !== 'expectedRevision').sort(),
                    units: (request.unitMutations ?? []).map((mutation) => mutation.key),
                    added: request.addCharacter?.chaId,
                    replaced: request.replaceCharacter?.chaId,
                })
                const result = await commit(request)
                historiesAfterCommit.push((await reader.readConversation(ownerId, selectedId))!.value.message)
                if (unsaved === 'a reply streamed during the addition' && request.addCharacter) streamReply()
                return result
            })

            // The sidebar's 새 캐릭터 생성 and 캐릭터 가져오기 add the character, then select it.
            const addedId = route === 'create'
                ? await characters.createNewCharacter()
                : await characters.commitDetachedCharacter(
                    syntheticCharacter('imported-character', importedHistory),
                    'import-character-card',
                )
            expect(selected().chaId).toBe(ownerId)
            expect(runtime.getSelectedConversationMode()).toBe('windowed')
            expect(get(app.localSaveFailure)).toBeNull()
            expect(committed.flatMap((entry) => entry.added ? [entry.added] : [])).toEqual([addedId])
            expect(committed.flatMap((entry) => entry.replaced ? [entry.replaced] : [])).toEqual([])
            expect(historiesAfterCommit.length).toBeGreaterThan(0)
            // The addition leaves the conversation alone; the same save then stores a streamed reply.
            const savedHistory = unsaved === 'a reply streamed during the addition' ? [...history, reply] : history
            expect(historiesAfterCommit[committed.findIndex((entry) => entry.added)]).toEqual(history)
            for (const messages of historiesAfterCommit) expect([history, savedHistory]).toContainEqual(messages)
            await reader.open()
            expect((await reader.readRoot()).value.characterOrder).toContain(addedId)
            if (unsaved === 'a reply streamed during the addition') expect(replyRowBeforeItsSave).toEqual(reply)
            if (unsaved !== 'nothing') {
                await runtime.flushPendingData('unsaved')
                expect(get(app.localSaveFailure)).toBeNull()
            }
            const additionCommitBoundary = committed.length

            stores.DBState.db.username = 'Saved beside the windowed chat'
            runtime.markPersistentDataDirty(1)
            await runtime.flushPendingData('settings-change')
            // A later save carries only its own change, not the added character again.
            const settingsCommits = committed.slice(additionCommitBoundary)
            expect(settingsCommits).toHaveLength(1)
            expect(settingsCommits[0].keys.filter((key) => key !== 'rootMutations' && key !== 'unitMutations')).toEqual([])
            expect(settingsCommits[0].units.filter((key) => key.includes(addedId))).toEqual([])

            expect(await characters.changeChar(indexOf(addedId))).toBe(true)
            expect(selected().chaId).toBe(addedId)
            stores.DBState.db.username = 'Saved after the addition'
            runtime.markPersistentDataDirty(1)
            await runtime.flushPendingData('settings-change')
            expect(get(app.localSaveFailure)).toBeNull()
            expect(await runtime.deactivateActiveWorkingSet()).toBe(true)
            expect(readConversation.mock.calls.filter(([, id]) => id === selectedId)).toEqual([])
            expect(consoleError.mock.calls.map((args) => args.map((value) =>
                value instanceof Error ? `${value.name}: ${value.message}` : String(value).slice(0, 200)))).toEqual([])

            await runtime.flushPendingData('exit')
            await reader.open()
            expect((await reader.readRoot()).revision).toBe(runtime.revision)
            expect((await reader.readRoot()).value.username).toBe('Saved after the addition')
            expect((await reader.readCharacter(addedId))?.value.chaId).toBe(addedId)
            if (route === 'import') {
                expect((await reader.readConversation(addedId, `${addedId}-chat`))?.value.message).toEqual(importedHistory)
            }
            expect((await reader.readConversation(ownerId, selectedId))?.value.message).toEqual(savedHistory)
        } finally {
            consoleError.mockRestore()
            Object.assign(globalThis, previousGlobals)
        }
    })

    // A reply keeps streaming into the open conversation while another change is stored: the reply
    // started before the commit and its text grows while the commit runs.
    it.each([
        { change: 'a save of another setting' },
        { change: 'a plugin settings write' },
        { change: 'a character detail change' },
        { change: 'a detail change of the open character' },
        { change: 'a chat setting change' },
        { change: 'a preset change' },
        { change: 'a plugin storage change' },
    ] as const)('keeps a reply that streams while $change is stored in the open windowed conversation', async ({ change }) => {
        const indexedDB = new IDBFactory()
        const previousGlobals = { indexedDB: globalThis.indexedDB, IDBKeyRange: globalThis.IDBKeyRange }
        const ownerId = 'edited-owner'
        const otherId = 'second-character'
        const selectedId = `${ownerId}-chat`
        const marker = 'Changed while streaming'
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
        const reader = new IndexedDbPersistentDataStore('risuai-persistent-data', indexedDB, IDBKeyRange)
        const history = Array.from({ length: 300 }, (_, index) => ({
            role: index % 2 === 0 ? 'user' : 'char',
            data: `Synthetic windowed message ${index}`,
            chatId: `synthetic-windowed-${index}`,
        })) as Message[]
        const library = syntheticLibrary('character')
        library.characters[0].chats[0].message = history
        try {
            const app = await bootProductionApp(indexedDB, library)
            const { stores, runtime, characters } = app
            const indexOf = (id: string) => stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === id)
            expect(await characters.changeChar(indexOf(ownerId))).toBe(true)
            expect(runtime.getSelectedConversationMode()).toBe('windowed')
            await runtime.flushPendingData('activation')
            const partial = { role: 'char', data: 'Synthetic reply', chatId: 'synthetic-reply' } as Message
            const full = { ...partial, data: 'Synthetic reply, streamed further' }
            const write = (start: number, deleteCount: number, message: Message) => {
                const target = runtime.captureSelectedConversationTarget()!
                const owner = stores.DBState.db.characters[indexOf(ownerId)]
                const chat = { ...cloneConversationMetadata(owner.chats.find((value) => value.id === selectedId)!), message: deleteCount ? [partial] : [] } as unknown as Chat
                const controller = runtime.captureWindowedConversationMutationController(target, chat, start)!
                expect(controller.applyRange(0, deleteCount, [message], deleteCount ? 'edit' : 'append')).toBe(true)
                controller.release()
            }
            write(history.length, 0, partial)

            // What the conversation shows, loaded from the store, before anything stores the grown text.
            let shown: { reply?: Message, first?: Message, total: number } | undefined
            const inspect = async () => {
                const source = runtime.getActiveConversationViewportSource()!
                await source.ensureRange({ startIndex: history.length - 2, limit: 4, reason: 'viewport' })
                await source.ensureRange({ startIndex: 0, limit: 8, reason: 'viewport' })
                const snapshot = source.snapshot()
                shown = { reply: snapshot.rowAt(history.length)?.message, first: snapshot.rowAt(0)?.message, total: snapshot.totalMessages }
            }
            const isChange = (request: object) => JSON.stringify(request).includes(change.includes('setting') && !change.includes('chat') ? 'loreBookDepth' : marker)
            let grown = false
            const commit = runtime.store.commit.bind(runtime.store)
            vi.spyOn(runtime.store, 'commit').mockImplementation(async (request) => {
                if (grown && !shown) await inspect()
                const result = await commit(request)
                if (!grown && isChange(request)) {
                    grown = true
                    write(history.length, 1, full)
                }
                return result
            })

            if (change === 'a save of another setting') {
                stores.DBState.db.loreBookDepth = 7
                runtime.markPersistentDataDirty(1)
                await runtime.flushPendingData('settings-change')
            } else if (change === 'a plugin settings write') {
                await runtime.commitPersistentUnitIntent('plugin-settings-write', [
                    { key: JSON.stringify(['root', 'loreBookDepth']), type: 'set', value: 7 },
                ])
            } else if (change === 'a character detail change') {
                expect(await runtime.mutateCharacterDetailFields(otherId, 'character-detail', ({ character }) => {
                    character.desc = marker
                }, () => stores.DBState.db.characters[indexOf(otherId)])).toBe(true)
            } else if (change === 'a detail change of the open character') {
                expect(await runtime.mutateCharacterDetailFields(ownerId, 'character-detail', ({ character }) => {
                    character.desc = marker
                }, () => stores.DBState.db.characters[indexOf(ownerId)])).toBe(true)
            } else if (change === 'a chat setting change') {
                await runtime.mutateConversationBinding(ownerId, selectedId, { note: marker }, () => {})
            } else if (change === 'a preset change') {
                await runtime.mutatePersistentPresets('preset-rename', (state) => {
                    expect(state.presets.length).toBeGreaterThan(0)
                    state.presets[0].name = marker
                })
            } else {
                await runtime.mutatePersistentPluginStorage('plugin-storage', [{ type: 'set', owner: 'synthetic-plugin', key: 'note', value: marker }])
            }
            expect(grown).toBe(true)
            if (!shown) await inspect()
            expect(get(app.localSaveFailure)).toBeNull()
            expect(runtime.getSelectedConversationMode()).toBe('windowed')
            expect(shown).toEqual({ reply: full, first: history[0], total: history.length + 1 })

            await runtime.flushPendingData('reply')
            expect(get(app.localSaveFailure)).toBeNull()
            await runtime.commitPersistentUnitIntent('plugin-settings-write', [
                { key: JSON.stringify(['root', 'loreBookDepth']), type: 'set', value: 9 },
            ])
            expect(consoleError.mock.calls.map((args) => args.map((value) =>
                value instanceof Error ? `${value.name}: ${value.message}` : String(value).slice(0, 200)))).toEqual([])
            await reader.open()
            expect((await reader.readConversation(ownerId, selectedId))?.value.message).toEqual([...history, full])
            expect((await reader.readRoot()).value.loreBookDepth).toBe(9)
        } finally {
            consoleError.mockRestore()
            Object.assign(globalThis, previousGlobals)
        }
    })

    it('keeps the saved body of a summarized chat when it moves into a folder', async () => {
        const indexedDB = new IDBFactory()
        const previousGlobals = { indexedDB: globalThis.indexedDB, IDBKeyRange: globalThis.IDBKeyRange }
        const ownerId = 'edited-owner'
        const selectedId = `${ownerId}-chat`
        const otherId = `${ownerId}-other`
        const reader = new IndexedDbPersistentDataStore('risuai-persistent-data', indexedDB, IDBKeyRange)
        const library = syntheticLibrary('character')
        const owner = library.characters[0] as character
        const otherLore = [{ key: 'other', comment: 'Other lore', content: 'kept', mode: 'normal', insertorder: 100, alwaysActive: false, secondkey: '', selective: false }]
        owner.chats.push({
            id: otherId,
            name: 'Other chat',
            note: 'Other note',
            localLore: otherLore,
            message: [{ role: 'user', data: 'Other history' }],
        } as unknown as Chat)
        owner.chatFolders = [{ id: 'folder-a', name: 'Folder A', folded: false }] as character['chatFolders']
        try {
            const { stores, runtime, characters, chatListRows } = await bootProductionApp(indexedDB, library)
            const selected = () => stores.DBState.db.characters[get(stores.selectedCharID)] as character
            expect(await characters.changeChar(
                stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === ownerId),
            )).toBe(true)
            expect(runtime.getSelectedConversationMode()).toBe('windowed')
            const other = selected().chats.find((chat) => chat.id === otherId)!
            // The resident copy is a summary without the saved note, lore or messages.
            expect(other).toMatchObject({ note: '', localLore: [], message: [] })
            const readConversation = vi.spyOn(runtime.store, 'readConversation')

            expect(await characters.editSelectedChatList(ownerId, 'reorder-chats', (character) => {
                const chats = chatListRows.orderChatsByDroppedRows(character.chats, [
                    { id: selectedId, folderId: null },
                    { id: otherId, folderId: 'folder-a' },
                ])
                if (!chats) return false
                character.chats = chats
                return null
            })).toBe(true)
            await runtime.flushPendingData('exit')
            expect(readConversation).not.toHaveBeenCalled()

            await reader.open()
            expect((await reader.readConversation(ownerId, otherId))?.value).toMatchObject({
                folderId: 'folder-a',
                note: 'Other note',
                localLore: otherLore,
                message: [{ role: 'user', data: 'Other history' }],
            })

            // The sidebar summary read after a restart shows the chat in its folder.
            const rebooted = await bootProductionApp(indexedDB)
            expect(await rebooted.characters.changeChar(
                rebooted.stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === ownerId),
            )).toBe(true)
            const reopened = rebooted.stores.DBState.db.characters[get(rebooted.stores.selectedCharID)] as character
            expect(reopened.chats.map((chat) => [chat.id, chat.folderId ?? null])).toEqual([
                [selectedId, null],
                [otherId, 'folder-a'],
            ])
            await rebooted.runtime.flushPendingData('exit')
        } finally {
            Object.assign(globalThis, previousGlobals)
        }
    })
    it('persists edits after committed replacement and captures only the active production working set', async () => {
        const indexedDB = new IDBFactory()
        const previousGlobals = { indexedDB: globalThis.indexedDB, IDBKeyRange: globalThis.IDBKeyRange }
        let app: ProductionApp | undefined
        try {
            app = await bootProductionApp(indexedDB, syntheticLibrary('character'))
            const selectOwner = async () => {
                expect(await app!.characters.changeChar(app!.stores.DBState.db.characters.findIndex(item => item.chaId === 'edited-owner'))).toBe(true)
            }
            await selectOwner()
            const inactiveIndex = app.stores.DBState.db.characters.findIndex(item => item.chaId === 'second-character')
            const inactive = app.stores.DBState.db.characters[inactiveIndex]
            // A non-plain test object retains its guard without Svelte replacing its descriptor.
            const guarded = Object.defineProperties(Object.create({}), Object.getOwnPropertyDescriptors(inactive))
            Object.defineProperty(guarded, 'chats', { configurable: true, enumerable: true, get: () => { throw new Error('inactive production capture') } })
            app.stores.DBState.db.characters[inactiveIndex] = guarded
            try {
                app.stores.DBState.db.username = 'Captured without inactive traversal'
                app.runtime.markPersistentDataDirty(1)
                await app.runtime.flushPendingDataLocally('synthetic-capture')
                expect((await app.runtime.store.readRoot()).value.username).toBe('Captured without inactive traversal')
            } finally { app.stores.DBState.db.characters[inactiveIndex] = inactive }

            const replacement = syntheticLibrary('character')
            replacement.characters[0].chats[0].message[0].data = 'Committed replacement'
            const compatibility = await app.runtime.acquireCompleteConversation('replacement-compatibility')
            try {
                const outcome = await app.runtime.replacePersistentDatabase(replacement, 'synthetic-replacement', { authoritative: true })
                expect(outcome.projection).toBe('applied')
            } finally { compatibility.release() }
            await selectOwner()
            const next = await app.runtime.acquireCompleteConversation('post-replacement-edit')
            const row = next.session.readRange(0, 1)
            expect(row.messages[0].data).toBe('Committed replacement')
            next.session.edit(row.locators[0], { ...row.messages[0], data: 'Edit after replacement' })
            await app.runtime.acknowledgeGenerationCompletion(app.runtime.getStorageAuthorityEpoch())
            next.release()
            await app.runtime.flushPendingData('exit')
            app = await bootProductionApp(indexedDB)
            await selectOwner()
            const reopened = await app.runtime.acquireCompleteConversation('reopen-contract')
            expect(reopened.session.readRange(0, 8).messages.map(message => message.data)).toEqual(['Edit after replacement'])
            reopened.release()
        } finally {
            await app?.runtime.flushPendingData('exit')
            Object.assign(globalThis, previousGlobals)
        }
    }, 30_000)

})

describe('HypaV3 always-on memory on character switch', () => {
    function hypaLibrary(alwaysToggleOn: boolean): Database {
        const library = syntheticLibrary('character')
        library.characters[1].supaMemory = false
        return Object.assign(library, {
            hypaV3: true,
            hypaV3PresetId: 0,
            hypaV3Presets: [{ name: 'Synthetic preset', settings: { alwaysToggleOn } }],
        })
    }

    async function withProductionApp(
        library: Database,
        run: (app: ProductionApp, reader: IndexedDbPersistentDataStore) => Promise<void>,
    ) {
        const indexedDB = new IDBFactory()
        const previousGlobals = { indexedDB: globalThis.indexedDB, IDBKeyRange: globalThis.IDBKeyRange }
        const consoleError = vi.spyOn(console, 'error').mockImplementation(() => {})
        try {
            const app = await bootProductionApp(indexedDB, library)
            await run(app, new IndexedDbPersistentDataStore('risuai-persistent-data', indexedDB, IDBKeyRange))
            expect(consoleError.mock.calls.map((args) => args.map((value) =>
                value instanceof Error ? `${value.name}: ${value.message}` : String(value).slice(0, 200)))).toEqual([])
        } finally {
            consoleError.mockRestore()
            Object.assign(globalThis, previousGlobals)
        }
    }

    function switchTo(app: ProductionApp, id: string) {
        const { characters, stores } = app
        return characters.changeChar(stores.DBState.db.characters.findIndex((candidate) => candidate.chaId === id))
    }

    function selected(app: ProductionApp) {
        return app.stores.DBState.db.characters[get(app.stores.selectedCharID)]
    }

    it('turns memory on while switching and saves it without loading the conversation', async () => {
        await withProductionApp(hypaLibrary(true), async (app, reader) => {
            const { runtime } = app
            const readConversation = vi.spyOn(runtime.store, 'readConversation')
            expect(await switchTo(app, 'edited-owner')).toBe(true)
            expect(runtime.getSelectedConversationMode()).toBe('windowed')
            expect(selected(app).supaMemory).toBe(true)
            expect(await switchTo(app, 'second-character')).toBe(true)
            expect(selected(app).supaMemory).toBe(true)
            await runtime.flushPendingData('exit')
            expect(readConversation).not.toHaveBeenCalled()
            await reader.open()
            expect((await reader.readCharacter('edited-owner'))?.value.supaMemory).toBe(true)
            expect((await reader.readCharacter('second-character'))?.value.supaMemory).toBe(true)
        })
    })

    it('turns memory on again on the next switch after it was turned off', async () => {
        await withProductionApp(hypaLibrary(true), async (app, reader) => {
            const { setCharacterMemory } = await import('../sidebarToggles')
            expect(await switchTo(app, 'edited-owner')).toBe(true)
            await setCharacterMemory(selected(app), false)
            expect(selected(app).supaMemory).toBe(false)
            expect(await switchTo(app, 'second-character')).toBe(true)
            await app.runtime.flushPendingData('switched-away')
            await reader.open()
            expect((await reader.readCharacter('edited-owner'))?.value.supaMemory).toBe(false)
            expect(await switchTo(app, 'edited-owner')).toBe(true)
            expect(selected(app).supaMemory).toBe(true)
            await app.runtime.flushPendingData('exit')
            expect((await reader.readCharacter('edited-owner'))?.value.supaMemory).toBe(true)
        })
    })

    it('leaves memory unchanged when the preset option is off', async () => {
        await withProductionApp(hypaLibrary(false), async (app, reader) => {
            expect(await switchTo(app, 'second-character')).toBe(true)
            expect(selected(app).supaMemory).toBe(false)
            await app.runtime.flushPendingData('exit')
            await reader.open()
            expect((await reader.readCharacter('second-character'))?.value.supaMemory).toBe(false)
        })
    })

    it('turns memory on for a character without conversations', async () => {
        const library = hypaLibrary(true)
        library.characters[1].chats = []
        await withProductionApp(library, async (app, reader) => {
            expect(await switchTo(app, 'second-character')).toBe(true)
            expect(app.runtime.getSelectedConversationMode()).toBe('complete')
            expect(selected(app).supaMemory).toBe(true)
            await app.runtime.flushPendingData('exit')
            await reader.open()
            expect((await reader.readCharacter('second-character'))?.value.supaMemory).toBe(true)
        })
    })

    it('does not change character data when only the selection store moves', () => {
        const database = hypaLibrary(true)
        setDatabaseLite(structuredClone(database))
        selectedCharID.set(1)
        expect(selIdState.selId).toBe(1)
        expect(getDatabase().characters[1].supaMemory).toBe(false)
    })
})
