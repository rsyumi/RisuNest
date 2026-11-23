import 'fake-indexeddb/auto'
import { afterEach, describe, expect, it, vi } from 'vitest'
import type { Database, Message } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import type { GeneratingConversation } from '../persistentDataStore'
import {
    capturePersistentRoot,
    capturePersistentPresets,
    createPersistentDataRuntime,
    publishPersistentCharacterMutationToWorkingSet,
    publishPersistentConversationReplacementToWorkingSet,
    type PersistentDataRuntimeStateAdapter,
} from '../persistentDataRuntime'
import { createCatalogPresetWorkingSet } from '../workingSetCatalog'
import { WorkingSetResidencyRegistry } from '../workingSetResidency'
import { ConversationSessionStaleError } from '../activeConversationSession'
import { createConversationPatchAccess } from '../../plugins/conversationPatchAccess'
import { createConversationOperationContext } from '../../process/conversationOperationContext'
import { createRisunestPrivateApi } from '../../plugins/apiV3/risunestPrivateApi'
import {
    isGenerationRequestPhaseOpen,
    openGenerationRequestPhase,
    trackConversationPatch,
} from '../../process/generationRequestPhase'

vi.mock('../database.svelte', () => ({
    getDatabase: () => {
        throw new Error('No live database in runtime integration tests')
    },
    presetTemplate: {},
}))
vi.mock('../../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))

const messages = (): Message[] => [
    { role: 'user', data: 'hello', chatId: 'm0' },
    { role: 'char', data: 'reply', chatId: 'm1', __tr: { text: 'old', parts: [1, 2] } } as Message,
    { role: 'user', data: 'again', chatId: 'm2' },
]

function makeDatabase(): Database {
    const chat = (id: string) => ({ id, name: id, note: '', localLore: [], message: messages(), scriptstate: { $a: 1 } })
    return {
        username: 'Fixture',
        characters: [
            { type: 'character', chaId: 'char-a', name: 'Alpha', chatPage: 0, chats: [chat('chat-a'), chat('chat-a2')] },
            { type: 'character', chaId: 'char-b', name: 'Beta', chatPage: 0, chats: [chat('chat-b')] },
        ],
    } as unknown as Database
}

function makeAdapter(database: Database): PersistentDataRuntimeStateAdapter & { current(): Database } {
    let workingCopy = structuredClone(database)
    const residency = new WorkingSetResidencyRegistry()
    residency.setEvictionAllowed(false)
    return {
        current: () => workingCopy,
        captureWorkingSetDatabase: () => workingCopy,
        captureCharacters: () => workingCopy.characters,
        captureRoot: () => capturePersistentRoot(workingCopy),
        capturePresets: () => capturePersistentPresets(workingCopy),
        capturePluginStorage: () => workingCopy.pluginCustomStorage ?? {},
        captureSelectedCharacter: () => structuredClone(workingCopy.characters[0] ?? null),
        captureCharacter: (id) => workingCopy.characters.find((item) => item.chaId === id) ?? null,
        getSelectedCharacterId: () => workingCopy.characters[0]?.chaId,
        replaceDatabase: (replacement) => { workingCopy = structuredClone(replacement) },
        publishPresetWorkingSet: ({ revision, presets }) => {
            const catalog = { revision, items: presets.map((preset, configuredIndex) => ({ id: preset['id'] as string, configuredIndex, name: preset.name ?? '', image: preset.image })) }
            workingCopy.botPresets = createCatalogPresetWorkingSet(catalog, null)
        },
        publishCharacter: (character) => {
            const index = workingCopy.characters.findIndex((item) => item.chaId === character.chaId)
            workingCopy.characters[index] = structuredClone(character)
        },
        publishCharacterMutation: (result) => {
            publishPersistentCharacterMutationToWorkingSet(workingCopy, result, residency, 0, vi.fn())
        },
        publishConversation: (characterId, conversation) => {
            const character = workingCopy.characters.find((item) => item.chaId === characterId)!
            const index = character.chats.findIndex((chat) => chat.id === conversation.id)
            character.chats[index] = structuredClone(conversation)
            character.chatPage = index
        },
        publishConversationReplacement: (result) => {
            publishPersistentConversationReplacementToWorkingSet(workingCopy, result)
        },
    }
}

function deferred() {
    let resolve!: () => void
    const promise = new Promise<void>((settle) => { resolve = settle })
    return { promise, resolve }
}

const selected = { characterId: 'char-a', conversationId: 'chat-a' }
const other = { characterId: 'char-b', conversationId: 'chat-b' }
const openPhases: Array<() => Promise<void>> = []

afterEach(async () => {
    while (openPhases.length) await openPhases.pop()!()
})

async function setup(database = makeDatabase()) {
    const name = `conversation-patch-${crypto.randomUUID()}`
    const store = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
    await store.open()
    await store.replaceFromDatabase(database)
    const adapter = makeAdapter(database)
    const runtime = createPersistentDataRuntime({ store, state: adapter, prepareDatabase: async (candidate) => structuredClone(candidate) })
    await runtime.initializeActiveWorkingSet(database)
    const generation = { target: null as GeneratingConversation | null, rerolls: new Set<string>(), windowed: false }
    const commitGate = { current: null as Promise<void> | null }
    const access = createConversationPatchAccess({
        flushPendingData: (reason) => runtime.flushPendingDataLocally(reason),
        getPersistentRevision: () => runtime.revision,
        captureSelectedConversationTarget: () => runtime.captureSelectedConversationTarget(),
        acquireCompleteConversation: (reason, target) => runtime.acquireCompleteConversation(reason, target),
        getActiveConversationSession: () => generation.windowed ? null : runtime.getActiveConversationSession(),
        getSelectedConversation: () => {
            const character = adapter.current().characters[0]
            return character.chats[character.chatPage ?? 0] ?? null
        },
        isConversationGenerating: (target) => generation.rerolls.has(target.conversationId) ||
            (generation.target?.characterId === target.characterId && generation.target.conversationId === target.conversationId),
        isGenerationRequestPhaseOpen,
        trackConversationPatch,
        commitPreparedUnitIntent: async (reason, prepare) => {
            await commitGate.current
            return runtime.commitPreparedUnitIntent(reason, prepare)
        },
    })
    const permission = { current: Promise.resolve(true) }
    const api = createRisunestPrivateApi({
        databaseAccess: { readConversationContext: vi.fn() },
        patchAccess: access,
        hostTools: { listTools: vi.fn(), callTool: vi.fn() },
        chatView: { register: vi.fn(), unregister: vi.fn(), dispose: vi.fn() },
        generationEnd: { register: vi.fn(), unregister: vi.fn(), dispose: vi.fn() },
        hasDatabasePermission: async () => true,
        hasDatabaseWritePermission: () => permission.current,
        hasHostToolPermission: async () => true,
        lifetimeSignal: new AbortController().signal,
    })
    const live = (target: GeneratingConversation) => adapter.current().characters
        .find((character) => character.chaId === target.characterId)!.chats
        .find((chat) => chat.id === target.conversationId)!
    const stored = async (target: GeneratingConversation) => (await store.readConversation(target.characterId, target.conversationId))!.value
    const reloaded = async (target: GeneratingConversation) => {
        const reopened = new IndexedDbPersistentDataStore(name, indexedDB, IDBKeyRange)
        await reopened.open()
        return (await reopened.readConversation(target.characterId, target.conversationId))!.value
    }
    const openRequestPhase = (target: GeneratingConversation) => {
        const close = openGenerationRequestPhase(target)
        openPhases.push(close)
        return close
    }
    const patch = (input: Record<string, unknown>) => api.patchConversation(input)
    return { store, adapter, runtime, generation, commitGate, permission, live, stored, reloaded, openRequestPhase, patch }
}

type Harness = Awaited<ReturnType<typeof setup>>
const paths = [['selected', selected], ['other', other]] as const

async function deleteFirstMessage(harness: Harness, target: GeneratingConversation) {
    if (target === selected) {
        const session = harness.runtime.getActiveConversationSession()!
        session.delete(session.locate(0))
        await harness.runtime.flushPendingDataLocally('test-delete')
    } else {
        await harness.runtime.commitPersistentUnitIntent('test-delete', [], [{ type: 'replace-range', ...target, start: 0, deleteCount: 1, messages: [] }])
    }
}

async function insertFirstMessage(harness: Harness, target: GeneratingConversation, message: Message) {
    if (target === selected) {
        const session = harness.runtime.getActiveConversationSession()!
        session.replaceRange(session.positionAt(0), 0, [message])
        await harness.runtime.flushPendingDataLocally('test-insert')
    } else {
        await harness.runtime.commitPersistentUnitIntent('test-insert', [], [{ type: 'replace-range', ...target, start: 0, deleteCount: 0, messages: [message] }])
    }
}

describe('risunestPatchConversation', () => {
    it('applies to the selected conversation through its session and replays the outcome', async () => {
        const harness = await setup()
        const session = harness.runtime.getActiveConversationSession()!
        const commands: string[] = []
        session.subscribe((event) => { if (event) commands.push(...event.commands) })
        const input = {
            ...selected,
            mutationId: 'selected-1',
            messages: [{ index: 1, messageId: 'm1', expected: { data: 'reply', __tr: { text: 'old', parts: [1, 2] }, __none: undefined }, set: { __tr: { text: 'new' } } }],
            chatVariables: [{ key: '$a', expected: 1, value: 2 }, { key: '$b', expected: null, value: 'x' }],
        }

        const result = await harness.patch(input)

        expect(result).toEqual({ status: 'applied', revision: harness.runtime.revision })
        expect(harness.live(selected).message[1]).toMatchObject({ data: 'reply', __tr: { text: 'new' } })
        expect(harness.live(selected).scriptstate).toEqual({ $a: 2, $b: 'x' })
        expect(commands).toEqual(['replace-range', 'update-metadata'])
        const persisted = await harness.reloaded(selected)
        expect(persisted.message[1]).toMatchObject({ __tr: { text: 'new' } })
        expect(persisted.scriptstate).toEqual({ $a: 2, $b: 'x' })
        await expect(harness.patch(input)).resolves.toEqual({ ...result, status: 'already-applied' })
        expect(harness.live(selected).scriptstate).toEqual({ $a: 2, $b: 'x' })
    })

    it('applies to another conversation through the store and shows it once selected', async () => {
        const harness = await setup()
        const sibling = { characterId: 'char-a', conversationId: 'chat-a2' }

        const result = await harness.patch({ ...sibling, mutationId: 'sibling', messages: [{ index: 1, messageId: 'm1', set: { __tr: 'sibling', data: 'edited' } }], chatVariables: [{ key: '$a', value: null }] })
        expect(result).toEqual({ status: 'applied', revision: harness.runtime.revision })
        await expect(harness.patch({ ...other, mutationId: 'other', messages: [{ index: 0, messageId: 'm0', set: { __tr: 'other' } }] }))
            .resolves.toMatchObject({ status: 'applied' })

        expect((await harness.reloaded(sibling)).message[1]).toMatchObject({ data: 'edited', __tr: 'sibling' })
        expect((await harness.reloaded(sibling)).scriptstate).toEqual({})
        expect((await harness.reloaded(other)).message[0]).toMatchObject({ __tr: 'other' })
        expect(await harness.runtime.activateConversation('chat-a2')).toBe(true)
        expect(harness.live(sibling).message[1]).toMatchObject({ data: 'edited', __tr: 'sibling' })
    })

    it.each(paths)('names the first failed target on the %s path and changes nothing', async (_path, target) => {
        const harness = await setup()
        const cases: Array<[Record<string, unknown>, Record<string, unknown>]> = [
            [{ messages: [{ index: 1, messageId: 'm1', expected: { data: 'other' }, set: { __x: 1 } }] }, { target: 'message', reason: 'mismatch', index: 1, field: 'data' }],
            [{ messages: [{ index: 1, messageId: 'm1', expected: { role: 'user' }, set: { __x: 1 } }] }, { target: 'message', reason: 'mismatch', index: 1, field: 'role' }],
            [{ messages: [{ index: 1, messageId: 'm1', expected: { __tr: { text: 'old', parts: [1, 3] } }, set: { __x: 1 } }] }, { target: 'message', reason: 'mismatch', index: 1, field: '__tr' }],
            [{ messages: [{ index: 1, messageId: 'm1', expected: { __tr: undefined }, set: { __x: 1 } }] }, { target: 'message', reason: 'mismatch', index: 1, field: '__tr' }],
            [{ messages: [{ index: 1, messageId: 'm0', set: { __x: 1 } }] }, { target: 'message', reason: 'mismatch', index: 1, field: 'chatId' }],
            [{ messages: [{ index: 3, messageId: 'm3', set: { __x: 1 } }] }, { target: 'message', reason: 'not-found', index: 3 }],
            [{ chatVariables: [{ key: '$a', expected: '1', value: 2 }] }, { target: 'chatVariable', reason: 'mismatch', key: '$a' }],
            [{ chatVariables: [{ key: '$a', expected: null, value: 2 }] }, { target: 'chatVariable', reason: 'mismatch', key: '$a' }],
            [{ messages: [{ index: 0, messageId: 'm0', set: { __x: 1 } }, { index: 2, messageId: 'm2', expected: { data: 'stale' }, set: { __x: 1 } }], chatVariables: [{ key: '$a', value: 9 }] },
                { target: 'message', reason: 'mismatch', index: 2, field: 'data' }],
        ]
        const before = await harness.stored(target)
        for (const [index, [input, conflict]] of cases.entries()) {
            const revision = harness.runtime.revision
            await expect(harness.patch({ ...target, mutationId: `conflict-${index}`, ...input }))
                .resolves.toEqual({ status: 'conflict', conflict, revision })
        }
        expect(await harness.stored(target)).toEqual(before)
        expect(harness.live(target).message).toEqual(before.message)
        expect(harness.live(target).scriptstate).toEqual({ $a: 1 })
        await expect(harness.patch({ ...target, mutationId: 'conflict-0', messages: [] }))
            .resolves.toMatchObject({ status: 'conflict', conflict: cases[0][1] })
    })

    it('reports a missing conversation', async () => {
        const harness = await setup()
        for (const target of [{ characterId: 'char-b', conversationId: 'missing' }, { characterId: 'missing', conversationId: 'chat-b' }]) {
            await expect(harness.patch({ ...target, mutationId: target.characterId + target.conversationId, chatVariables: [{ key: '$a', value: 1 }] }))
                .resolves.toEqual({ status: 'conflict', conflict: { target: 'conversation', reason: 'not-found' }, revision: harness.runtime.revision })
        }
    })

    it.each(paths)('addresses a repeated or missing message ID only at the read revision on the %s path', async (_path, target) => {
        const database = makeDatabase()
        const conversation = database.characters.find((value) => value.chaId === target.characterId)!.chats[0]
        conversation.message = [
            { role: 'user', data: 'first', chatId: 'a' },
            { role: 'char', data: 'same', chatId: 'dup' },
            { role: 'char', data: 'same', chatId: 'dup' },
            { role: 'user', data: 'unnamed' },
        ]
        const harness = await setup(database)
        let revision = harness.runtime.revision

        await expect(harness.patch({ ...target, mutationId: 'dup-1', baseRevision: revision, messages: [{ index: 2, messageId: 'dup', set: { __x: 'second' } }] }))
            .resolves.toMatchObject({ status: 'applied' })
        revision = harness.runtime.revision
        await expect(harness.patch({ ...target, mutationId: 'unnamed', baseRevision: revision, messages: [{ index: 3, messageId: null, set: { __x: 'unnamed' } }] }))
            .resolves.toMatchObject({ status: 'applied' })
        expect((await harness.stored(target)).message.map((message) => (message as unknown as Record<string, unknown>).__x))
            .toEqual([undefined, undefined, 'second', 'unnamed'])

        await expect(harness.patch({ ...target, mutationId: 'no-base', messages: [{ index: 1, messageId: 'dup', set: { __x: 1 } }] }))
            .rejects.toThrow(/baseRevision/)
        await expect(harness.patch({ ...target, mutationId: 'null-no-base', messages: [{ index: 3, messageId: null, set: { __x: 1 } }] }))
            .rejects.toThrow(/baseRevision/)

        revision = harness.runtime.revision
        await deleteFirstMessage(harness, target)
        await expect(harness.patch({ ...target, mutationId: 'after-delete', baseRevision: revision, messages: [{ index: 1, messageId: 'dup', set: { __x: 'moved' } }] }))
            .resolves.toMatchObject({ status: 'conflict', conflict: { target: 'conversation', reason: 'revision' } })
        revision = harness.runtime.revision
        await insertFirstMessage(harness, target, { role: 'user', data: 'inserted', chatId: 'inserted' })
        await expect(harness.patch({ ...target, mutationId: 'after-insert', baseRevision: revision, messages: [{ index: 1, messageId: 'dup', set: { __x: 'moved' } }] }))
            .resolves.toMatchObject({ status: 'conflict', conflict: { target: 'conversation', reason: 'revision' } })
        expect((await harness.stored(target)).message.map((message) => (message as unknown as Record<string, unknown>).__x))
            .toEqual([undefined, undefined, 'second', 'unnamed'])
    })

    it.each(paths)('returns a mismatch for a moved message with a unique ID on the %s path', async (_path, target) => {
        const harness = await setup()
        await deleteFirstMessage(harness, target)

        await expect(harness.patch({ ...target, mutationId: 'moved', messages: [{ index: 1, messageId: 'm1', set: { __x: 1 } }] }))
            .resolves.toMatchObject({ status: 'conflict', conflict: { target: 'message', reason: 'mismatch', index: 1, field: 'chatId' } })
    })

    it.each(paths)('keeps the selected response variant snapshot in step on the %s path', async (_path, target) => {
        const database = makeDatabase()
        const conversation = database.characters.find((value) => value.chaId === target.characterId)!.chats[0]
        conversation.message = [
            { role: 'user', data: 'hello', chatId: 'm0' },
            { role: 'char', data: 'first part', chatId: 'r0', saying: 'x' },
            {
                role: 'char', data: 'second part', chatId: 'r1', saying: 'y',
                responseVariants: { groupId: 'group', selectedId: 'one', candidates: [
                    { id: 'one', messages: [{ role: 'char', data: 'first part', chatId: 'r0', saying: 'x' }, { role: 'char', data: 'second part', chatId: 'r1', saying: 'y' }] },
                    { id: 'two', messages: [{ role: 'char', data: 'other', chatId: 'o0' }] },
                ] },
            } as Message,
        ]
        const harness = await setup(database)

        await expect(harness.patch({ ...target, mutationId: 'variant', messages: [{ index: 1, messageId: 'r0', set: { __tr: 'translated' } }] }))
            .resolves.toMatchObject({ status: 'applied' })

        const [, first, carrier] = (await harness.stored(target)).message
        expect(first).toMatchObject({ __tr: 'translated' })
        expect(carrier.responseVariants!.candidates[0].messages[0]).toMatchObject({ chatId: 'r0', __tr: 'translated' })
        expect(carrier.responseVariants!.candidates[1].messages).toEqual([{ role: 'char', data: 'other', chatId: 'o0' }])
    })

    it('keeps both a racing user edit and the patch', async () => {
        const harness = await setup()
        const session = harness.runtime.getActiveConversationSession()!
        const flushed = deferred()
        harness.commitGate.current = flushed.promise
        const sibling = { characterId: 'char-a', conversationId: 'chat-a2' }

        const pending = harness.patch({ ...selected, mutationId: 'race', messages: [{ index: 1, messageId: 'm1', set: { __tr: 'patched' } }] })
        const pendingStore = harness.patch({ ...sibling, mutationId: 'race-store', messages: [{ index: 1, messageId: 'm1', expected: { data: 'reply' }, set: { __tr: 'patched' } }] })
        session.edit(session.locate(1), { ...session.readMessage(session.locate(1)), data: 'user edit' })
        await harness.runtime.commitPersistentUnitIntent('test-edit', [], [{ type: 'replace-range', ...sibling, start: 1, deleteCount: 1, messages: [{ role: 'char', data: 'other edit', chatId: 'm1' }] }])
        flushed.resolve()

        await expect(pending).resolves.toMatchObject({ status: 'applied' })
        await expect(pendingStore).resolves.toMatchObject({ status: 'conflict', conflict: { field: 'data' } })
        expect((await harness.reloaded(selected)).message[1]).toMatchObject({ data: 'user edit', __tr: 'patched' })
        expect((await harness.reloaded(sibling)).message[1]).toEqual({ role: 'char', data: 'other edit', chatId: 'm1' })
    })
})

describe('risunestPatchConversation racing a Lua conversation commit', () => {
    it('fails the stale Lua commit loudly and lets a later patch apply on top of the Lua change', async () => {
        const harness = await setup()
        const session = harness.runtime.getActiveConversationSession()!
        const stale = createConversationOperationContext(session, harness.live(selected))
        stale.chat.message[1] = { ...stale.chat.message[1], data: 'lua edit' }

        await expect(harness.patch({ ...selected, mutationId: 'before-lua', messages: [{ index: 1, messageId: 'm1', set: { __tr: 'patched' } }] }))
            .resolves.toMatchObject({ status: 'applied' })
        expect(() => stale.commit(session)).toThrow(ConversationSessionStaleError)
        expect(harness.live(selected).message[1]).toMatchObject({ data: 'reply', __tr: 'patched' })

        const lua = createConversationOperationContext(session, harness.live(selected))
        lua.chat.message[1] = { ...lua.chat.message[1], data: 'lua edit' }
        lua.commit(session)
        await expect(harness.patch({ ...selected, mutationId: 'after-lua', messages: [{ index: 1, messageId: 'm1', set: { __tr: 'after' } }] }))
            .resolves.toMatchObject({ status: 'applied' })
        expect((await harness.reloaded(selected)).message[1]).toMatchObject({ data: 'lua edit', __tr: 'after' })
    })
})

describe('risunestPatchConversation during a generation', () => {
    it('applies plugin fields during the request without moving the continuation point', async () => {
        const harness = await setup()
        const session = harness.runtime.getActiveConversationSession()!
        const chat = harness.live(selected)
        harness.generation.target = selected
        const close = harness.openRequestPhase(selected)
        const version = session.version
        const invalidation = session.generationInvalidationVersion
        chat.toolCalls = { call: { call: { id: 'call', name: 'tool', arg: {} }, response: [] } } as never

        const result = await harness.patch({ ...selected, mutationId: 'request', messages: [{ index: 1, messageId: 'm1', set: { __tr: 'during' } }], chatVariables: [{ key: '$a', value: 3 }] })
        expect(result).toEqual({ status: 'applied', revision: harness.runtime.revision })

        expect(session.version).toBe(version + 1)
        expect(session.generationInvalidationVersion).toBe(invalidation)
        expect(session.canContinueGenerationFrom(version)).toBe(true)
        expect(harness.runtime.getActiveConversationSession()).toBe(session)
        expect(harness.live(selected)).toBe(chat)
        expect(session.materializeCompatibilityArray()).toBe(chat.message)
        expect(chat.message[1]).toMatchObject({ __tr: 'during' })
        expect(chat.scriptstate).toEqual({ $a: 3 })
        await close()
        session.append({ role: 'char', data: 'generated reply', chatId: 'm3' })
        harness.generation.target = null
        await harness.runtime.flushPendingDataLocally('generation-end')

        const persisted = await harness.reloaded(selected)
        expect(persisted.message[1]).toMatchObject({ __tr: 'during' })
        expect(persisted.message.at(-1)).toMatchObject({ data: 'generated reply' })
        expect(persisted.scriptstate).toEqual({ $a: 3 })
        expect(persisted.toolCalls).toMatchObject({ call: { call: { name: 'tool' } } })
    })

    it('returns busy at once outside the request phase, for data, and for a windowed conversation', async () => {
        const harness = await setup()
        harness.generation.target = selected
        const fields = { messages: [{ index: 1, messageId: 'm1', set: { __tr: 'x' } }] }
        const before = await harness.stored(selected)
        const revision = harness.runtime.revision

        await expect(harness.patch({ ...selected, mutationId: 'prompt', ...fields })).resolves.toEqual({ status: 'busy', revision })
        harness.openRequestPhase(selected)
        await expect(harness.patch({ ...selected, mutationId: 'data', messages: [{ index: 1, messageId: 'm1', set: { data: 'x' } }] }))
            .resolves.toEqual({ status: 'busy', revision })
        harness.generation.windowed = true
        await expect(harness.patch({ ...selected, mutationId: 'windowed', ...fields })).resolves.toEqual({ status: 'busy', revision })
        harness.generation.windowed = false
        await openPhases.pop()!()
        await expect(harness.patch({ ...selected, mutationId: 'output', ...fields })).resolves.toEqual({ status: 'busy', revision })

        expect(await harness.stored(selected)).toEqual(before)
        expect(harness.runtime.revision).toBe(revision)
    })

    it('applies a busy patch after the generation with the same mutation ID', async () => {
        const harness = await setup()
        const input = { ...selected, mutationId: 'retry', messages: [{ index: 1, messageId: 'm1', set: { data: 'rewritten' } }] }
        harness.generation.target = selected

        await expect(harness.patch(input)).resolves.toMatchObject({ status: 'busy' })
        harness.generation.target = null
        await expect(harness.patch(input)).resolves.toMatchObject({ status: 'applied' })
        expect(harness.live(selected).message[1].data).toBe('rewritten')
    })

    it('applies to another conversation during the generation', async () => {
        const harness = await setup()
        harness.generation.target = selected

        await expect(harness.patch({ ...other, mutationId: 'elsewhere', messages: [{ index: 1, messageId: 'm1', set: { data: 'elsewhere' } }] }))
            .resolves.toMatchObject({ status: 'applied' })
        expect((await harness.stored(other)).message[1].data).toBe('elsewhere')
    })

    it('treats a running reroll as a generation', async () => {
        const harness = await setup()
        harness.generation.rerolls.add(selected.conversationId)
        const fields = { messages: [{ index: 1, messageId: 'm1', set: { __tr: 'reroll' } }] }

        await expect(harness.patch({ ...selected, mutationId: 'reroll-prompt', ...fields })).resolves.toMatchObject({ status: 'busy' })
        harness.openRequestPhase(selected)
        await expect(harness.patch({ ...selected, mutationId: 'reroll-request', ...fields })).resolves.toMatchObject({ status: 'applied' })
        expect(harness.live(selected).message[1]).toMatchObject({ __tr: 'reroll' })
    })

    it('keeps the generation waiting for a request-phase patch, but not for a permission answer', async () => {
        const harness = await setup()
        harness.generation.target = selected
        const close = harness.openRequestPhase(selected)
        const gate = deferred()
        harness.commitGate.current = gate.promise
        let closed = false

        const pending = harness.patch({ ...selected, mutationId: 'slow', messages: [{ index: 1, messageId: 'm1', set: { __tr: 'slow' } }] })
        // The patch joins the request phase once its permission answer is in.
        for (let tick = 0; tick < 20; tick++) await Promise.resolve()
        let answer!: (granted: boolean) => void
        harness.permission.current = new Promise<boolean>((resolve) => { answer = resolve })
        const unanswered = harness.patch({ ...selected, mutationId: 'unanswered', messages: [{ index: 1, messageId: 'm1', set: { __tr: 'unanswered' } }] })
        harness.permission.current = Promise.resolve(true)
        const closing = close().then(() => { closed = true })
        for (let tick = 0; tick < 20; tick++) await Promise.resolve()
        expect(isGenerationRequestPhaseOpen(selected)).toBe(false)
        expect(closed).toBe(false)
        await expect(harness.patch({ ...selected, mutationId: 'late', messages: [{ index: 1, messageId: 'm1', set: { __tr: 'late' } }] }))
            .resolves.toMatchObject({ status: 'busy' })
        gate.resolve()

        await expect(pending).resolves.toMatchObject({ status: 'applied' })
        await closing
        answer(true)
        await expect(unanswered).resolves.toMatchObject({ status: 'busy' })
        expect(harness.live(selected).message[1]).toMatchObject({ __tr: 'slow' })
    })
})
