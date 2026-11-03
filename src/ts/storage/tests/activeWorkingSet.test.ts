import { IDBFactory, IDBKeyRange, IDBObjectStore } from 'fake-indexeddb'
import { describe, expect, it, vi } from 'vitest'
import { writeHistoryWindow } from '../../process/historyWindowWrite'
import { ActiveWorkingSet } from '../activeWorkingSet.svelte'
import type { Chat, Database, Message, character } from '../database.svelte'
import { IndexedDbPersistentDataStore } from '../indexedDbPersistentDataStore'
import {
    capturePersistentRoot,
    createPersistentDataRuntime,
} from '../persistentDataRuntime'
import { isCatalogCharacterStub } from '../workingSetCatalog'
import { WorkingSetResidencyRegistry } from '../workingSetResidency'
import type { PersistentConversationMetadata } from '../persistentDataStore'
import { createLeaseStub } from '../persistentDataStore.testSupport'
import { fixtureDatabase } from './persistentDataFixtures'
import { createPersistentStoreHarness } from '../../../../tests/support/persistentStoreHarness'

function makeCharacter(id: string, chats: Chat[] = []): character {
    return {
        type: 'character',
        chaId: id,
        name: id.toUpperCase(),
        chats,
    } as unknown as character
}

function makeChat(id: string): Chat {
    return { id, name: id, note: '', localLore: [], message: [] }
}

function deferred<T>() {
    let resolve!: (value: T) => void
    let reject!: (error: unknown) => void
    const promise = new Promise<T>((resolvePromise, rejectPromise) => {
        resolve = resolvePromise
        reject = rejectPromise
    })
    return { promise, resolve, reject }
}

async function makeHarness(
    options: {
        characters?: character[]
        hydrateFullCharacter?: boolean
        conversationPageSize?: number
    } = {},
) {
    const previous = makeCharacter('previous', [makeChat('previous-chat')])
    const database = {
        username: 'Fixture',
        characters: [structuredClone(previous)],
    } as unknown as Database
    const storage = await createPersistentStoreHarness({
        username: 'Fixture',
        botPresets: [],
        characters: [previous, ...(options.characters ?? [makeCharacter('char-a')])],
    } as unknown as Database)
    const readCharacter = storage.spy('readCharacter')
    const queryConversations = storage.spy('queryConversations')
    const readConversation = storage.spy('readConversation')
    const acquireRevision = storage.spy('acquireRevision').mockRejectedValue(
        new Error('navigation acquired a snapshot'),
    )
    const open = storage.spy('open')
    let selectedCharacterId = 'previous'
    const coordinator = {
        revision: storage.revision,
        mutationGeneration: 0,
        initialize: vi.fn(),
        flushPendingData: vi.fn(() => Promise.resolve()),
        retireWindowedSelectedConversation: vi.fn(),
        replacePersistentDatabase: vi.fn(async () => ({
            kind: 'committed', revision: storage.revision, projection: 'applied',
        } as const)),
        adoptHydratedCharacter: vi.fn(() => true),
        markPersistentDataDirty: vi.fn(),
        recordActiveConversationMutation: vi.fn(),
    }
    const publishedCharacters: character[] = []
    const publishedConversations: Array<{ characterId: string; conversation: Chat }> = []
    const publishCharacter = vi.fn((value: character) => {
        selectedCharacterId = value.chaId
        publishedCharacters.push(value as character)
    })
    const releaseInactiveCharacter = vi.fn()
    let releaseAllowed = true
    let workingSetActivationAllowed = true
    let workingSetReleaseAllowed = true
    const workingSet = new ActiveWorkingSet({
        store: storage.store,
        coordinator: coordinator as never,
        getSelectedCharacterId: () => selectedCharacterId,
        getResidentCharacter: (id) =>
            database.characters.find((character) => character.chaId === id) ?? null,
        publishCharacter,
        publishConversation: (characterId, conversation, nextCharacter) => {
            if (nextCharacter) {
                const index = database.characters.findIndex(
                    (character) => character.chaId === characterId,
                )
                if (index >= 0) database.characters[index] = nextCharacter
            }
            publishedConversations.push({ characterId, conversation })
        },
        canActivateWorkingSet: () => workingSetActivationAllowed,
        canDeactivateWorkingSet: () => workingSetReleaseAllowed,
        canDeactivateCharacter: () => releaseAllowed,
        releaseInactiveCharacter,
        shouldHydrateFullCharacter: () => options.hydrateFullCharacter === true,
        canReleaseConversation: (character, conversationId) =>
            releaseAllowed && !character.chats.find(
                (conversation) => conversation.id === conversationId,
            )?.isStreaming,
        conversationPageSize: options.conversationPageSize,
    })
    return {
        workingSet,
        storage,
        revision: storage.revision,
        readCharacter,
        queryConversations,
        readConversation,
        acquireRevision,
        open,
        coordinator,
        database,
        publishedCharacters,
        publishedConversations,
        publishCharacter,
        releaseInactiveCharacter,
        setSelectedCharacterId(id: string) {
            selectedCharacterId = id
        },
        setReleaseAllowed(allowed: boolean) {
            releaseAllowed = allowed
        },
        setWorkingSetActivationAllowed(allowed: boolean) {
            workingSetActivationAllowed = allowed
        },
        setWorkingSetReleaseAllowed(allowed: boolean) {
            workingSetReleaseAllowed = allowed
        },
    }
}

async function makeWindowedHarness(input: {
    characters: character[]
    selectedCharacterId?: string | null
    adoptionResult?: boolean
}) {
    const database = {
        username: 'Windowed fixture',
        characters: structuredClone(input.characters),
    } as unknown as Database
    const storage = await createPersistentStoreHarness({
        username: 'Windowed fixture',
        botPresets: [],
        characters: structuredClone(input.characters),
    } as unknown as Database)
    const store = storage.store
    let selectedCharacterId = input.selectedCharacterId ?? null
    let windowedAllowed = true
    let operationActive = false
    // Windowed activation reads bounded windows; a full conversation read fails it.
    const readConversation = storage.spy('readConversation').mockRejectedValue(
        new Error('windowed activation performed a full conversation read'),
    )
    const readConversationMetadata = storage.spy('readConversationMetadata')
    const readCharacter = storage.spy('readCharacter')
    let workingSet!: ActiveWorkingSet
    const coordinator = {
        revision: 1,
        mutationGeneration: 0,
        hasPendingPersistenceWork: false,
        initialize: vi.fn(),
        flushPendingData: vi.fn(async () => undefined),
        retireWindowedSelectedConversation: vi.fn(),
        replacePersistentDatabase: vi.fn(async () => undefined),
        adoptHydratedCharacter: vi.fn(() => true),
        runSelectedConversationTransition: vi.fn(<T>(transition: () => T) =>
            transition(),
        ),
        adoptWindowedSelectedConversation: vi.fn(
            () => input.adoptionResult ?? true,
        ),
        advanceWindowedSelectedConversationRevision: vi.fn(() => true),
        markPersistentDataDirty: vi.fn(),
        recordActiveConversationMutation: vi.fn(),
        recordWindowedChatListChange: vi.fn(() => true),
    }
    const publishCharacter = vi.fn((next: character) => {
        const index = database.characters.findIndex(
            (candidate) => candidate.chaId === next.chaId,
        )
        if (index < 0) database.characters.push(next)
        else database.characters[index] = next
        selectedCharacterId = next.chaId
    })
    const publishConversation = vi.fn(
        (
            characterId: string,
            conversation: Chat,
            nextCharacter?: character,
        ) => {
            const index = database.characters.findIndex(
                (candidate) => candidate.chaId === characterId,
            )
            if (index < 0)
                throw new Error('published conversation owner is missing')
            if (nextCharacter) database.characters[index] = nextCharacter
            else {
                const owner = database.characters[index]
                const conversationIndex = owner.chats.findIndex(
                    (candidate) => candidate.id === conversation.id,
                )
                owner.chats[conversationIndex] = conversation
                owner.chatPage = conversationIndex
            }
            selectedCharacterId = characterId
        },
    )
    const captureActivationRollback = vi.fn(
        (characterIds: readonly string[]) => {
            const selectedBefore = selectedCharacterId
            const entries = new Map(
                characterIds.map((id) => [
                    id,
                    database.characters.find(
                        (candidate) => candidate.chaId === id,
                    ) ?? null,
                ]),
            )
            return () => {
                for (const [id, entry] of entries) {
                    const index = database.characters.findIndex(
                        (candidate) => candidate.chaId === id,
                    )
                    if (entry && index >= 0) database.characters[index] = entry
                }
                selectedCharacterId = selectedBefore
            }
        },
    )
    workingSet = new ActiveWorkingSet({
        store,
        coordinator:
            coordinator as unknown as import('../activeWorkingSet.svelte').WorkingSetCoordinator,
        getSelectedCharacterId: () => selectedCharacterId,
        getResidentCharacter: (id) =>
            database.characters.find((candidate) => candidate.chaId === id) ??
            null,
        publishCharacter,
        publishConversation,
        captureActivationRollback,
        canActivateWorkingSet: () => true,
        canUseWindowedSelectedConversation: () => windowedAllowed,
        isConversationOperationActive: () => operationActive,
        shouldHydrateFullCharacter: () => false,
        canReleaseConversation: () => true,
        conversationViewportRowBudget: 32,
    })
    return {
        workingSet,
        database,
        coordinator,
        storage,
        readCharacter,
        readConversation,
        readConversationMetadata,
        captureActivationRollback,
        publishConversation,
        /** Commits an unrelated root edit, as background persistence would, and returns the new revision. */
        async commitBackgroundEdit() {
            const { revision } = await store.readRoot()
            return (await store.commit({
                expectedRevision: revision,
                rootMutations: [{ type: 'set', key: 'username', value: `Background ${revision}` }],
            })).revision
        },
        setWindowedAllowed(allowed: boolean) {
            windowedAllowed = allowed
        },
        setOperationActive(active: boolean) {
            operationActive = active
        },
    }
}

describe('ActiveWorkingSet', () => {
    async function activateWindowedChatList() {
        const selected = makeChat('chat-a')
        selected.message = [
            { role: 'user', data: 'first', chatId: 'a' },
            { role: 'char', data: 'second', chatId: 'b' },
        ] as Message[]
        const other = makeChat('chat-b')
        other.message = [{ role: 'user', data: 'other', chatId: 'c' }] as Message[]
        const harness = await makeWindowedHarness({ characters: [makeCharacter('a', [selected, other])] })
        await expect(harness.workingSet.activateCharacter('a')).resolves.toBe(true)
        expect(harness.workingSet.selectedConversationMode).toBe('windowed')
        const resident = harness.database.characters[0]
        return {
            harness,
            resident,
            shell: resident.chats[0],
            target: harness.workingSet.captureSelectedConversationTarget()!,
        }
    }

    it('applies a windowed chat-list edit around the selected shell object', async () => {
        const { harness, resident, shell, target } = await activateWindowedChatList()
        const added = makeChat('added')

        expect(harness.workingSet.editWindowedChatList(target, (character) => {
            character.chats.unshift(added)
            character.chats.reverse()
            return 'added'
        })).toEqual({ kind: 'applied', nextId: 'added' })

        expect(resident.chats.map((chat) => chat.id)).toEqual(['chat-b', 'chat-a', 'added'])
        expect(resident.chats[1]).toBe(shell)
        expect(resident.chatPage).toBe(1)
        const [authority, before, after] =
            harness.coordinator.recordWindowedChatListChange.mock.calls[0] as unknown as [
                { conversationId: string }, character, character,
            ]
        expect(authority.conversationId).toBe('chat-a')
        expect(before).toBe(resident)
        expect(after.chats.map((chat) => chat.id)).toEqual(['chat-b', 'chat-a', 'added'])
        expect(after.chatPage).toBe(1)
        expect(harness.readConversation).not.toHaveBeenCalled()
    })

    it('leaves the working set untouched when a chat-list edit cannot be recorded', async () => {
        const { harness, resident, target } = await activateWindowedChatList()
        harness.coordinator.recordWindowedChatListChange.mockReturnValueOnce(false)
        const chats = resident.chats

        expect(harness.workingSet.editWindowedChatList(target, (character) => {
            character.chats.unshift(makeChat('added'))
            character.chats[1].folderId = 'folder'
            return null
        })).toEqual({ kind: 'unsupported' })

        expect(resident.chats).toBe(chats)
        expect(resident.chats.map((chat) => chat.id)).toEqual(['chat-a', 'chat-b'])
        expect(resident.chats[0].folderId).toBeUndefined()
        expect(resident.chatPage ?? 0).toBe(0)
    })

    it('leaves chat-list edits to the complete path while a conversation operation runs', async () => {
        const { harness, target } = await activateWindowedChatList()
        harness.setOperationActive(true)
        const edit = vi.fn(() => null)

        expect(harness.workingSet.editWindowedChatList(target, edit)).toEqual({ kind: 'unsupported' })

        expect(edit).not.toHaveBeenCalled()
        expect(harness.coordinator.recordWindowedChatListChange).not.toHaveBeenCalled()
    })

    it('edits a middle row with bounded reads and unchanged viewport ownership', async () => {
        const full = makeChat('chat-a')
        full.message = Array.from({ length: 5000 }, (_, index) => ({ role: 'user', data: 'x'.repeat(4096), chatId: String(index) })) as Message[]
        const harness = await makeWindowedHarness({ characters: [makeCharacter('a', [full])] })
        await harness.workingSet.activateCharacter('a')
        const source = harness.workingSet.activeConversationViewportSource
        const target = harness.workingSet.captureSelectedConversationTarget()!
        const controller = await harness.workingSet.captureWindowedMessageMutation(target, 123, full.message[123])
        expect(controller).not.toBeNull()
        expect(controller!.applyRange(0, 1, [{ ...full.message[123], data: 'edited' }], 'edit')).toBe(true)
        expect(harness.workingSet.activeConversationViewportSource).toBe(source)
        expect(harness.readConversation).not.toHaveBeenCalled()
        expect(harness.coordinator.recordActiveConversationMutation).toHaveBeenCalledWith(expect.objectContaining({ mutations: [{ start: 123, deleteCount: 1, messages: [{ ...full.message[123], data: 'edited' }], sessionVersion: 1 }] }))
        expect(controller!.applyRange(0, 0, [full.message[123]], 'append')).toBe(false)
        controller!.release()
    })

    it('refuses a middle-row editor whose original evidence changed', async () => {
        const { harness, target } = await activateWindowedChatList()
        await expect(harness.workingSet.captureWindowedMessageMutation(target, 0, { role: 'user', data: 'stale', chatId: 'a' })).resolves.toBeNull()
        expect(harness.coordinator.recordActiveConversationMutation).not.toHaveBeenCalled()
        expect(harness.readConversation).not.toHaveBeenCalled()
    })

    it.each(['metadata', 'content', 'navigation'] as const)('adopts persisted window metadata with %s identity checks', async (change) => {
        const full = makeChat('chat-a')
        full.message = [
            { role: 'user', data: 'outside', chatId: 'a' },
            { role: 'user', data: 'tail', chatId: 'b' },
        ] as Message[]
        const harness = await makeWindowedHarness({ characters: [makeCharacter('a', [full])] })
        await harness.workingSet.activateCharacter('a')
        const target = harness.workingSet.captureSelectedConversationTarget()!
        const local = { ...structuredClone(full), message: [structuredClone(full.message[1])] }
        const controller = harness.workingSet.captureWindowedConversationMutationController(target, local, 1)!
        const reader = createLeaseStub({
            revision: 2,
            readConversation: vi.fn(),
            readConversationWindow: vi.fn(async () => ({
                revision: 2,
                value: {
                    characterId: 'a', conversationId: 'chat-a', startIndex: 1, endIndex: 2, totalMessages: 2,
                    hasMoreBefore: true, hasMoreAfter: false,
                    messages: [{ ...full.message[1], data: change === 'content' ? 'changed' : 'tail', __plugin: 'stored' }],
                },
            })),
        })
        const adopt = await harness.workingSet.prepareWindowedMetadataAdoption(reader)
        if (change === 'navigation') harness.workingSet.fenceNavigation()
        else harness.workingSet.advanceStoreRevision(2)
        adopt()
        expect(reader.readConversationWindow).toHaveBeenCalledExactlyOnceWith({
            characterId: 'a', conversationId: 'chat-a', startIndex: 1, limit: 1,
        })
        expect(reader.readConversation).not.toHaveBeenCalled()
        if (change === 'metadata') {
            expect(local.message[0]).toMatchObject({ data: 'tail', __plugin: 'stored' })
            expect(controller.applyRange(1, 0, [{ role: 'char', data: 'reply', chatId: 'c' }], 'append')).toBe(true)
        } else {
            expect(local.message[0]).not.toHaveProperty('__plugin')
            expect(controller.isCurrent()).toBe(false)
        }
        controller.release()
    })

    it('fences a tail controller after another controller makes a same-length edit', async () => {
        const full = makeChat('chat-a')
        full.message = [
            { role: 'user', data: 'covered', chatId: 'a' },
            { role: 'user', data: 'tail', chatId: 'b' },
        ] as Message[]
        const harness = await makeWindowedHarness({ characters: [makeCharacter('a', [full])] })
        await harness.workingSet.activateCharacter('a')
        const target = harness.workingSet.captureSelectedConversationTarget()!
        const local = { ...structuredClone(full), message: [structuredClone(full.message[1])] }
        const first = harness.workingSet.captureWindowedConversationMutationController(target, local, 1)!
        const other = harness.workingSet.captureWindowedConversationMutationController(target, structuredClone(local), 1)!
        expect(other.applyRange(0, 1, [{ role: 'user', data: 'external edit', chatId: 'b' }], 'edit')).toBe(true)
        expect(first.applyRange(0, 1, [{ role: 'user', data: 'stale edit', chatId: 'b' }], 'edit')).toBe(false)
        expect(local.message[0].data).toBe('tail')
    })

    it('records bounded generation tail mutations without promoting the conversation', async () => {
        const full = makeChat('chat-a')
        full.message = [
            { role: 'user', data: 'covered-a', chatId: 'a' },
            { role: 'char', data: 'covered-b', chatId: 'b' },
            { role: 'user', data: 'tail', chatId: 'c' },
        ] as Message[]
        const harness = await makeWindowedHarness({ characters: [makeCharacter('a', [full])] })
        await expect(harness.workingSet.activateCharacter('a')).resolves.toBe(true)
        const target = harness.workingSet.captureSelectedConversationTarget()!
        const local = { ...structuredClone(full), message: [structuredClone(full.message[2])] }
        const controller = harness.workingSet.captureWindowedConversationMutationController(
            target,
            local,
            2,
        )!

        expect(controller.applyRange(1, 0, [{
            role: 'char', data: '', chatId: 'generated',
        }], 'append')).toBe(true)
        expect(harness.workingSet.acknowledgeConversationMutationPersisted({
            characterId: 'a',
            conversationId: 'chat-a',
            sessionToken: harness.workingSet.captureSelectedConversationAuthority()!.sessionToken,
            sessionVersion: 1,
            revision: 2,
        })).toBe(true)
        harness.workingSet.advanceStoreRevision(2)
        expect(controller.applyRange(1, 1, [{
            role: 'char', data: 'complete', chatId: 'generated',
        }], 'edit')).toBe(true)

        expect(Object.getOwnPropertyDescriptor(
            harness.database.characters[0].chats[0],
            'message',
        )?.get).toBeTypeOf('function')
        expect(harness.workingSet.captureSelectedConversationAuthority()).toMatchObject({
            totalMessages: 4,
            persistedSessionVersion: 1,
            sessionVersion: 2,
        })
        expect(harness.coordinator.recordActiveConversationMutation.mock.calls.map(
            ([event]) => event.mutations[0],
        )).toMatchObject([
            { start: 3, deleteCount: 0, sessionVersion: 1 },
            { start: 3, deleteCount: 1, sessionVersion: 2 },
        ])
        expect(harness.workingSet.acknowledgeConversationMutationPersisted({
            characterId: 'a',
            conversationId: 'chat-a',
            sessionToken: harness.workingSet.captureSelectedConversationAuthority()!.sessionToken,
            sessionVersion: 2,
            revision: 2,
        })).toBe(true)
        expect(harness.workingSet.captureSelectedConversationAuthority()).toMatchObject({
            persistedSessionVersion: 2,
        })
        expect(harness.readConversation).not.toHaveBeenCalled()
    })

    it('writes a script removal inside a generation window through the windowed tail', async () => {
        const full = makeChat('chat-a')
        full.message = [
            { role: 'user', data: 'covered', chatId: 'a' },
            { role: 'char', data: 'removed', chatId: 'b' },
            { role: 'user', data: 'kept', chatId: 'c' },
            { role: 'char', data: 'tail', chatId: 'd' },
        ] as Message[]
        const harness = await makeWindowedHarness({ characters: [makeCharacter('a', [full])] })
        await expect(harness.workingSet.activateCharacter('a')).resolves.toBe(true)
        const target = harness.workingSet.captureSelectedConversationTarget()!
        const local = { ...structuredClone(full), message: structuredClone(full.message.slice(1)) }
        const controller = harness.workingSet.captureWindowedConversationMutationController(target, local, 1)!

        expect(writeHistoryWindow(controller, [local.message[1], local.message[2]])).toBe(true)

        expect(local.message.map((message) => message.data)).toEqual(['kept', 'tail'])
        expect(harness.workingSet.captureSelectedConversationAuthority()).toMatchObject({ totalMessages: 3 })
        expect(harness.workingSet.activeConversationViewportSource!.snapshot().totalMessages).toBe(3)
        expect(harness.coordinator.recordActiveConversationMutation.mock.calls[0][0].mutations[0])
            .toMatchObject({ start: 1, deleteCount: 3, messages: [{ data: 'kept' }, { data: 'tail' }] })
        controller.release()
    })

    it('rolls back a bounded tail mutation when persistence rejects its evidence', async () => {
        const full = makeChat('chat-a')
        full.message = [
            { role: 'user', data: 'covered', chatId: 'a' },
            { role: 'char', data: 'tail', chatId: 'b' },
        ] as Message[]
        const harness = await makeWindowedHarness({ characters: [makeCharacter('a', [full])] })
        await expect(harness.workingSet.activateCharacter('a')).resolves.toBe(true)
        const target = harness.workingSet.captureSelectedConversationTarget()!
        const local = { ...structuredClone(full), message: [structuredClone(full.message[1])] }
        const controller = harness.workingSet.captureWindowedConversationMutationController(
            target,
            local,
            1,
        )!
        const authorityBefore = harness.workingSet.captureSelectedConversationAuthority()
        harness.coordinator.recordActiveConversationMutation.mockImplementationOnce(() => {
            throw new Error('Synthetic persistence rejection')
        })

        expect(() => controller.applyRange(1, 0, [{
            role: 'char', data: 'generated', chatId: 'generated',
        }], 'append')).toThrow('Synthetic persistence rejection')
        expect(local.message).toEqual([{ role: 'char', data: 'tail', chatId: 'b' }])
        expect(harness.workingSet.captureSelectedConversationAuthority()).toEqual(authorityBefore)
    })

    it('propagates a windowed transition failure after restoring the previous selection', async () => {
        const harness = await makeWindowedHarness({
            characters: [makeCharacter('a', [makeChat('chat-a')]), makeCharacter('b', [makeChat('chat-b')])],
        })
        await expect(harness.workingSet.activateCharacter('a')).resolves.toBe(true)
        const previous = harness.database.characters[0]
        const error = new Error('Synthetic pending persistence failure')
        harness.coordinator.runSelectedConversationTransition.mockImplementationOnce(() => { throw error })
        await expect(harness.workingSet.activateCharacter('b')).rejects.toBe(error)
        expect(harness.database.characters[0]).toBe(previous)
        expect(harness.workingSet.captureSelectedConversationTarget()).toMatchObject({
            characterId: 'a', conversationId: 'chat-a',
        })
        await expect(harness.workingSet.activateCharacter('b')).resolves.toBe(true)
    })

    it('waits for background persistence queued during hydration before publishing', async () => {
        const harness = await makeWindowedHarness({
            characters: [makeCharacter('a', [makeChat('chat-a')])],
        })
        const { coordinator } = harness
        let reads = 0
        const readCharacter = harness.storage.observe('readCharacter', () => {
            if (reads++ === 0) coordinator.hasPendingPersistenceWork = true
        })
        coordinator.flushPendingData.mockImplementation(async () => {
            coordinator.hasPendingPersistenceWork = false
        })
        coordinator.runSelectedConversationTransition.mockImplementation(<T>(transition: () => T) => {
            if (coordinator.hasPendingPersistenceWork) throw new Error('Selected conversation authority transition has pending persistence')
            return transition()
        })

        await expect(harness.workingSet.activateCharacter('a')).resolves.toBe(true)
        expect(coordinator.flushPendingData).toHaveBeenCalledTimes(2)
        expect(readCharacter).toHaveBeenCalledOnce()
        expect(harness.workingSet.captureSelectedConversationTarget()).toMatchObject({
            characterId: 'a', conversationId: 'chat-a',
        })
    })

    it('does not publish a new selection when persistence remains busy after settling', async () => {
        const harness = await makeWindowedHarness({
            characters: [makeCharacter('a', [makeChat('chat-a')]), makeCharacter('b', [makeChat('chat-b')])],
        })
        await expect(harness.workingSet.activateCharacter('a')).resolves.toBe(true)
        const previous = harness.database.characters[0]
        harness.storage.observe('readCharacter', () => {
            harness.coordinator.hasPendingPersistenceWork = true
        })
        harness.coordinator.runSelectedConversationTransition.mockClear()
        harness.coordinator.runSelectedConversationTransition.mockImplementation(<T>(transition: () => T) => {
            if (harness.coordinator.hasPendingPersistenceWork) throw new Error('pending persistence')
            return transition()
        })

        let exhaustedGeneration: number | undefined
        await expect(harness.workingSet.activateCharacter('b', {
            onRetryExhausted: (generation) => { exhaustedGeneration = generation },
        })).resolves.toBe(false)
        expect(exhaustedGeneration).toBe(harness.workingSet.navigationGenerationToken)
        expect(harness.coordinator.runSelectedConversationTransition).not.toHaveBeenCalled()
        expect(harness.database.characters[0]).toBe(previous)
        expect(harness.workingSet.captureSelectedConversationTarget()?.characterId).toBe('a')
    })

    it('reads the character again when background persistence moves the revision', async () => {
        const harness = await makeWindowedHarness({
            characters: [makeCharacter('a', [makeChat('chat-a')])],
        })
        const { coordinator } = harness
        let reads = 0
        const readCharacter = harness.storage.observe('readCharacter', () => {
            if (reads++ === 0) coordinator.hasPendingPersistenceWork = true
        })
        coordinator.flushPendingData.mockImplementation(async () => {
            if (!coordinator.hasPendingPersistenceWork) return
            coordinator.hasPendingPersistenceWork = false
            coordinator.revision = await harness.commitBackgroundEdit()
        })
        coordinator.runSelectedConversationTransition.mockImplementation(<T>(transition: () => T) => {
            if (coordinator.hasPendingPersistenceWork) throw new Error('Selected conversation authority transition has pending persistence')
            return transition()
        })

        await expect(harness.workingSet.activateCharacter('a')).resolves.toBe(true)
        expect(readCharacter).toHaveBeenCalledTimes(2)
        expect(harness.workingSet.captureSelectedConversationAuthority()).toMatchObject({
            characterId: 'a', conversationId: 'chat-a', storeRevision: 2,
        })
    })

    it('directly activates a large selected conversation from metadata', async () => {
        const conversation = {
            ...makeChat('chat-large'),
            note: 'preserved note',
            fmIndex: 7,
            message: Array.from({ length: 10_000 }, (_, index) => ({
                role: 'user',
                data: `message-${index}`,
            })),
        } as Chat
        const target = makeCharacter('char-large', [conversation])
        const harness = await makeWindowedHarness({ characters: [target] })

        await expect(
            harness.workingSet.activateCharacter('char-large'),
        ).resolves.toBe(true)

        expect(harness.readConversation).not.toHaveBeenCalled()
        expect(harness.readConversationMetadata).toHaveBeenCalledOnce()
        expect(harness.workingSet.selectedConversationMode).toBe('windowed')
        expect(harness.workingSet.activeConversationSession).toBeNull()
        expect(
            harness.workingSet.captureSelectedConversationAuthority(),
        ).toMatchObject({
            characterId: 'char-large',
            conversationId: 'chat-large',
            totalMessages: 10_000,
        })
        expect(
            harness.workingSet.activeConversationViewportSource?.snapshot(),
        ).toMatchObject({
            totalMessages: 10_000,
        })
        expect(
            harness.workingSet.activeConversationViewportSource
                ?.snapshot()
                .rowAt(0),
        ).toBeUndefined()
        const selected = harness.database.characters[0].chats[0]
        expect(selected.note).toBe('preserved note')
        expect(selected.fmIndex).toBe(7)
        expect(() => selected.message).toThrow('metadata-only')
    })

    it('fences captured targets while retaining the selected viewport owner', async () => {
        const target = makeCharacter('char-a', [
            {
                ...makeChat('chat-a'),
                message: [{ role: 'user', data: 'persisted' }],
            } as Chat,
        ])
        const harness = await makeWindowedHarness({ characters: [target] })
        await harness.workingSet.activateCharacter('char-a')
        const staleTarget =
            harness.workingSet.captureSelectedConversationTarget()!
        const source = harness.workingSet.activeConversationViewportSource

        const generation = harness.workingSet.fenceNavigation()
        const currentTarget =
            harness.workingSet.captureSelectedConversationTarget()!

        expect(generation).toBe(staleTarget.navigationGeneration + 1)
        expect(currentTarget.navigationGeneration).toBe(generation)
        expect(currentTarget).not.toEqual(staleTarget)
        expect(harness.workingSet.activeConversationViewportSource).toBe(source)
        expect(
            harness.workingSet.captureSelectedConversationAuthority(),
        ).not.toBeNull()
        await expect(
            harness.workingSet.acquireCompleteConversation(
                'stale',
                staleTarget,
            ),
        ).rejects.toBeInstanceOf(Error)
    })

    it('switches windowed conversations without full reads and keeps repeat selection usable', async () => {
        const chatA = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'first' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'second' }],
        } as Chat
        const target = makeCharacter('char-a', [chatA, chatB])
        const harness = await makeWindowedHarness({ characters: [target] })
        await harness.workingSet.activateCharacter('char-a')
        const previousSource =
            harness.workingSet.activeConversationViewportSource!

        await expect(
            harness.workingSet.activateConversation('chat-b'),
        ).resolves.toBe(true)
        await expect(
            harness.workingSet.activateConversation('chat-b'),
        ).resolves.toBe(true)

        expect(harness.readConversation).not.toHaveBeenCalled()
        expect(harness.publishConversation).toHaveBeenCalledOnce()
        expect(previousSource.snapshot().totalMessages).toBe(0)
        expect(
            harness.workingSet.captureSelectedConversationAuthority(),
        ).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-b',
            totalMessages: 1,
        })
        expect(harness.database.characters[0].chatPage).toBe(1)
        expect(() => harness.database.characters[0].chats[1].message).toThrow(
            'metadata-only',
        )
    })

    it('keeps the previous windowed owner usable when destination adoption is rejected', async () => {
        const characterA = makeCharacter('char-a', [
            {
                ...makeChat('chat-a'),
                message: [{ role: 'user', data: 'first' }],
            } as Chat,
        ])
        const characterB = makeCharacter('char-b', [
            {
                ...makeChat('chat-b'),
                message: [{ role: 'char', data: 'second' }],
            } as Chat,
        ])
        const harness = await makeWindowedHarness({
            characters: [characterA, characterB],
        })
        await harness.workingSet.activateCharacter('char-a')
        const previousSource =
            harness.workingSet.activeConversationViewportSource!
        harness.coordinator.adoptWindowedSelectedConversation.mockReturnValueOnce(
            false,
        )

        await expect(
            harness.workingSet.activateCharacter('char-b'),
        ).resolves.toBe(false)

        expect(harness.workingSet.activeConversationViewportSource).toBe(
            previousSource,
        )
        expect(previousSource.snapshot().totalMessages).toBe(1)
        expect(
            harness.workingSet.captureSelectedConversationTarget(),
        ).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-a',
        })
        expect(harness.database.characters[0].chaId).toBe('char-a')
        expect(harness.database.characters[1].chats[0].message).toEqual(
            characterB.chats[0].message,
        )
    })

    it('restores the post-flush owner when adoption rejects after its revision advances', async () => {
        const characterA = makeCharacter('char-a', [
            {
                ...makeChat('chat-a'),
                message: [{ role: 'user', data: 'first' }],
            } as Chat,
        ])
        const characterB = makeCharacter('char-b', [
            {
                ...makeChat('chat-b'),
                message: [{ role: 'char', data: 'second' }],
            } as Chat,
        ])
        const harness = await makeWindowedHarness({
            characters: [characterA, characterB],
        })
        await harness.workingSet.activateCharacter('char-a')
        const preFlushSource =
            harness.workingSet.activeConversationViewportSource!
        let postFlushSource = preFlushSource
        await harness.commitBackgroundEdit()
        harness.coordinator.flushPendingData.mockImplementationOnce(
            async () => {
                harness.coordinator.revision = 2
                harness.workingSet.advanceStoreRevision(2)
                postFlushSource =
                    harness.workingSet.activeConversationViewportSource!
            },
        )
        harness.coordinator.adoptWindowedSelectedConversation.mockReturnValueOnce(
            false,
        )

        await expect(
            harness.workingSet.activateCharacter('char-b'),
        ).resolves.toBe(false)

        expect(preFlushSource.snapshot().totalMessages).toBe(0)
        expect(harness.workingSet.activeConversationViewportSource).toBe(
            postFlushSource,
        )
        expect(postFlushSource.snapshot()).toMatchObject({
            storeRevision: 2,
            totalMessages: 1,
        })
        expect(
            harness.workingSet.captureSelectedConversationTarget(),
        ).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-a',
            storeRevision: 2,
        })
    })

    it('does not let a stale complete fallback replace a newer character navigation', async () => {
        const characters = ['a', 'b', 'c'].map((suffix) =>
            makeCharacter(`char-${suffix}`, [
                {
                    ...makeChat(`chat-${suffix}`),
                    message: [{ role: 'user', data: suffix }],
                } as Chat,
            ]),
        )
        const harness = await makeWindowedHarness({ characters })
        await harness.workingSet.activateCharacter('char-a')
        let newerNavigation: Promise<boolean> | null = null

        const staleNavigation = harness.workingSet.activateCharacter('char-b', {
            normalize(candidate) {
                harness.setWindowedAllowed(false)
                queueMicrotask(() => {
                    harness.setWindowedAllowed(true)
                    newerNavigation =
                        harness.workingSet.activateCharacter('char-c')
                })
                return candidate
            },
        })

        await expect(staleNavigation).resolves.toBe(false)
        await vi.waitFor(() => expect(newerNavigation).not.toBeNull())
        await expect(newerNavigation!).resolves.toBe(true)
        expect(harness.readConversation).not.toHaveBeenCalled()
        expect(
            harness.workingSet.captureSelectedConversationTarget(),
        ).toMatchObject({
            characterId: 'char-c',
            conversationId: 'chat-c',
        })
    })

    it('discards delayed metadata when the mutation generation advances', async () => {
        const chatA = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'first' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'second' }],
        } as Chat
        const harness = await makeWindowedHarness({
            characters: [makeCharacter('char-a', [chatA, chatB])],
        })
        await harness.workingSet.activateCharacter('char-a')
        const pending = deferred<{
            revision: number
            value: PersistentConversationMetadata
        } | null>()
        harness.readConversationMetadata.mockImplementationOnce(
            () => pending.promise,
        )
        const activation = harness.workingSet.activateConversation('chat-b')
        await vi.waitFor(() =>
            expect(harness.readConversationMetadata).toHaveBeenCalledTimes(2),
        )
        harness.coordinator.mutationGeneration = 1
        const { message, ...conversation } = structuredClone(chatB)
        pending.resolve({
            revision: 1,
            value: {
                characterId: 'char-a',
                conversationId: 'chat-b',
                conversation,
                totalMessages: message.length,
            },
        })

        await expect(activation).resolves.toBe(false)
        expect(
            harness.workingSet.captureSelectedConversationTarget(),
        ).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-a',
        })
    })

    it('passes exact detached normalization evidence and schedules persistence after adoption', async () => {
        const target = {
            ...makeCharacter('char-a', [
                {
                    ...makeChat('chat-a'),
                    note: 'before',
                    message: [{ role: 'user', data: 'persisted' }],
                } as Chat,
            ]),
            lastInteraction: 1,
        } as character
        const harness = await makeWindowedHarness({ characters: [target] })

        await expect(
            harness.workingSet.activateCharacter('char-a', {
                normalize(candidate) {
                    candidate.lastInteraction = 2
                    candidate.chats[0].note = 'after'
                    return candidate
                },
            }),
        ).resolves.toBe(true)

        expect(
            harness.coordinator.adoptWindowedSelectedConversation,
        ).toHaveBeenCalledWith(
            1,
            0,
            harness.database.characters[0],
            expect.objectContaining({ conversationId: 'chat-a' }),
            {
                character: {
                    before: expect.objectContaining({
                        chaId: 'char-a',
                        lastInteraction: 1,
                    }),
                    after: expect.objectContaining({
                        chaId: 'char-a',
                        lastInteraction: 2,
                    }),
                },
                conversation: {
                    before: expect.objectContaining({
                        id: 'chat-a',
                        note: 'before',
                    }),
                    after: expect.objectContaining({
                        id: 'chat-a',
                        note: 'after',
                    }),
                },
            },
        )
        expect(
            harness.coordinator.markPersistentDataDirty,
        ).toHaveBeenCalledOnce()
        expect(
            harness.coordinator.markPersistentDataDirty.mock
                .invocationCallOrder[0],
        ).toBeGreaterThan(
            harness.coordinator.runSelectedConversationTransition.mock
                .invocationCallOrder[0],
        )
    })
    it('hydrates only the selected conversation body in the scalable working set', async () => {
        const chatA = {
            ...makeChat('chat-a'),
            folderId: 'folder-a',
            bindedPersona: 'persona-a',
            message: [{ role: 'user', data: 'inactive body' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'user', data: 'selected body' }],
        } as Chat
        const harness = await makeHarness({
            characters: [{ ...makeCharacter('char-a', [chatA, chatB]), chatPage: 1 } as character],
        })

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(true)

        expect(harness.readConversation).toHaveBeenCalledOnce()
        expect(harness.readConversation).toHaveBeenCalledWith('char-a', 'chat-b')
        expect(harness.publishedCharacters[0]).toMatchObject({
            chaId: 'char-a',
            chatPage: 1,
            chats: [
                {
                    id: 'chat-a',
                    name: 'chat-a',
                    folderId: 'folder-a',
                    bindedPersona: 'persona-a',
                    message: [],
                },
                { id: 'chat-b', name: 'chat-b', message: chatB.message },
            ],
        })
    })

    it('flushes before releasing all active characters on leave', async () => {
        const harness = await makeHarness()
        await harness.workingSet.activateCharacter('char-a')
        harness.releaseInactiveCharacter.mockClear()
        harness.coordinator.flushPendingData.mockClear()

        await expect(harness.workingSet.deactivate()).resolves.toBe(true)

        expect(harness.releaseInactiveCharacter).toHaveBeenCalledWith('char-a')
        expect(harness.coordinator.flushPendingData).toHaveBeenCalledWith(
            'deactivate-working-set',
        )
        expect(harness.coordinator.flushPendingData.mock.invocationCallOrder[0]).toBeLessThan(
            harness.coordinator.retireWindowedSelectedConversation.mock.invocationCallOrder[0],
        )
        expect(harness.coordinator.retireWindowedSelectedConversation.mock.invocationCallOrder[0]).toBeLessThan(
            harness.releaseInactiveCharacter.mock.invocationCallOrder[0],
        )
        expect([...harness.workingSet.activeCharacterIds]).toEqual([])
    })

    it('keeps dirty active characters resident when leave flush fails', async () => {
        const harness = await makeHarness()
        await harness.workingSet.activateCharacter('char-a')
        harness.releaseInactiveCharacter.mockClear()
        harness.coordinator.flushPendingData.mockRejectedValueOnce(new Error('flush failed'))

        await expect(harness.workingSet.deactivate()).rejects.toThrow('flush failed')

        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
        expect(harness.coordinator.retireWindowedSelectedConversation).not.toHaveBeenCalled()
        expect([...harness.workingSet.activeCharacterIds]).toEqual(['char-a'])
    })

    it('settles persistence queued as the leave flush completes before releasing characters', async () => {
        const harness = await makeWindowedHarness({ characters: [makeCharacter('a', [makeChat('chat-a')])] })
        await harness.workingSet.activateCharacter('a')
        harness.coordinator.flushPendingData.mockImplementationOnce(async () => {
            harness.coordinator.hasPendingPersistenceWork = true
        }).mockImplementationOnce(async () => {
            harness.coordinator.hasPendingPersistenceWork = false
        })
        harness.coordinator.retireWindowedSelectedConversation.mockImplementation(() => {
            if (harness.coordinator.hasPendingPersistenceWork) throw new Error('Selected conversation ownership has pending persistence')
        })

        await expect(harness.workingSet.deactivate()).resolves.toBe(true)

        expect([...harness.workingSet.activeCharacterIds]).toEqual([])
        expect(harness.workingSet.captureSelectedConversationTarget()).toBeNull()
    })

    it('retains the selected conversation when persistence stays busy while leaving', async () => {
        const harness = await makeWindowedHarness({ characters: [makeCharacter('a', [makeChat('chat-a')])] })
        await harness.workingSet.activateCharacter('a')
        harness.coordinator.flushPendingData.mockImplementation(async () => {
            harness.coordinator.hasPendingPersistenceWork = true
        })

        await expect(harness.workingSet.deactivate()).resolves.toBe(false)

        expect([...harness.workingSet.activeCharacterIds]).toEqual(['a'])
        expect(harness.workingSet.captureSelectedConversationTarget()).toMatchObject({
            characterId: 'a', conversationId: 'chat-a',
        })
        expect(harness.coordinator.retireWindowedSelectedConversation).not.toHaveBeenCalled()
    })

    it('keeps a streaming character active until a later leave after settlement', async () => {
        const harness = await makeHarness()
        await harness.workingSet.activateCharacter('char-a')
        harness.releaseInactiveCharacter.mockClear()
        harness.setReleaseAllowed(false)

        await expect(harness.workingSet.deactivate()).resolves.toBe(false)

        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
        expect([...harness.workingSet.activeCharacterIds]).toEqual(['char-a'])

        harness.setReleaseAllowed(true)
        await expect(harness.workingSet.deactivate()).resolves.toBe(true)
        expect(harness.releaseInactiveCharacter).toHaveBeenCalledWith('char-a')
        expect([...harness.workingSet.activeCharacterIds]).toEqual([])
    })

    it('keeps the selected conversation target current when leave becomes blocked during flush', async () => {
        const harness = await makeHarness({
            characters: [makeCharacter('char-a', [makeChat('chat-a')])],
            hydrateFullCharacter: true,
        })
        await harness.workingSet.activateCharacter('char-a')
        const flushing = deferred<void>()
        harness.coordinator.flushPendingData.mockReturnValueOnce(flushing.promise)

        const leaving = harness.workingSet.deactivate()
        harness.setWorkingSetReleaseAllowed(false)
        flushing.resolve()

        await expect(leaving).resolves.toBe(false)
        harness.setWorkingSetReleaseAllowed(true)
        const target = harness.workingSet.captureSelectedConversationTarget()
        expect(target).not.toBeNull()
        const lease = await harness.workingSet.acquireCompleteConversation(
            'after-blocked-leave',
            target!,
        )
        lease.release()
    })

    it('keeps the selected conversation target current when activation becomes blocked during flush', async () => {
        const harness = await makeHarness({
            characters: [makeCharacter('char-a', [makeChat('chat-a'), makeChat('chat-b')])],
            hydrateFullCharacter: true,
        })
        await harness.workingSet.activateCharacter('char-a')
        const flushing = deferred<void>()
        harness.coordinator.flushPendingData.mockReturnValueOnce(flushing.promise)

        const activating = harness.workingSet.activateConversation('chat-b')
        harness.setWorkingSetActivationAllowed(false)
        flushing.resolve()

        await expect(activating).resolves.toBe(false)
        harness.setWorkingSetActivationAllowed(true)
        const target = harness.workingSet.captureSelectedConversationTarget()
        expect(target).not.toBeNull()
        const lease = await harness.workingSet.acquireCompleteConversation(
            'after-blocked-activation',
            target!,
        )
        lease.release()
    })

    it('keeps the active character resident while generation is busy before streaming starts', async () => {
        const harness = await makeHarness()
        await harness.workingSet.activateCharacter('char-a')
        harness.releaseInactiveCharacter.mockClear()
        harness.coordinator.flushPendingData.mockClear()
        harness.setWorkingSetReleaseAllowed(false)

        await expect(harness.workingSet.deactivate()).resolves.toBe(false)

        expect(harness.coordinator.flushPendingData).not.toHaveBeenCalled()
        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
        expect([...harness.workingSet.activeCharacterIds]).toEqual(['char-a'])
    })

    it('allows maximum compatibility to leave while retaining complete data', async () => {
        const harness = await makeHarness()
        await harness.workingSet.activateCharacter('char-a')
        harness.releaseInactiveCharacter.mockClear()
        harness.releaseInactiveCharacter.mockReturnValueOnce(false)

        await expect(harness.workingSet.deactivate()).resolves.toBe(true)

        expect(harness.releaseInactiveCharacter).toHaveBeenCalledWith('char-a')
        expect([...harness.workingSet.activeCharacterIds]).toEqual([])
    })

    it('hydrates every conversation body for a maximum-compatibility pin', async () => {
        const chats = [
            {
                ...makeChat('chat-a'),
                message: [{ role: 'user', data: 'full A' }],
            } as Chat,
            {
                ...makeChat('chat-b'),
                message: [{ role: 'char', data: 'full B' }],
            } as Chat,
        ]
        // One conversation per page, so both bodies arrive only if every page is read.
        const harness = await makeHarness({
            characters: [makeCharacter('char-a', chats)],
            hydrateFullCharacter: true,
            conversationPageSize: 1,
        })

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(true)

        expect(harness.publishedCharacters[0]).toMatchObject({
            chaId: 'char-a',
            chats,
        })
        expect(harness.readConversation).toHaveBeenCalledTimes(2)
        expect(harness.queryConversations).toHaveBeenCalledTimes(2)
        expect(harness.acquireRevision).not.toHaveBeenCalled()
        expect(harness.coordinator.adoptHydratedCharacter).toHaveBeenCalledWith(
            harness.revision,
            0,
            harness.publishedCharacters[0],
        )
        expect(
            harness.coordinator.adoptHydratedCharacter.mock.invocationCallOrder[0],
        ).toBeLessThan(harness.publishCharacter.mock.invocationCallOrder[0])
    })

    it('releases the previous character only after the hydrated target is published', async () => {
        const harness = await makeHarness({
            characters: [makeCharacter('char-a', [makeChat('chat-a')])],
        })

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(true)

        expect(harness.releaseInactiveCharacter).toHaveBeenCalledWith('previous')
        expect(harness.coordinator.flushPendingData.mock.invocationCallOrder[0]).toBeLessThan(
            harness.releaseInactiveCharacter.mock.invocationCallOrder[0],
        )
        expect(harness.publishCharacter.mock.invocationCallOrder[0]).toBeLessThan(
            harness.releaseInactiveCharacter.mock.invocationCallOrder[0],
        )
    })

    it('bounds full character detail while visiting A then B then C', async () => {
        const database = {
            characters: ['a', 'b', 'c'].map((id) => ({
                ...makeCharacter(`char-${id}`, [makeChat(`chat-${id}`)]),
                personality: `body-${id}`,
            })),
        } as unknown as Database
        const storage = await createPersistentStoreHarness({
            username: 'Fixture',
            botPresets: [],
            characters: structuredClone(database.characters),
        } as unknown as Database)
        let selectedCharacterId = 'char-a'
        const residency = new WorkingSetResidencyRegistry()
        const workingSet = new ActiveWorkingSet({
            store: storage.store,
            coordinator: {
                revision: storage.revision,
                mutationGeneration: 0,
                initialize: vi.fn(),
                flushPendingData: vi.fn(async () => undefined),
                replacePersistentDatabase: vi.fn(async () => undefined),
                adoptHydratedCharacter: vi.fn(() => true),
                markPersistentDataDirty: vi.fn(),
            },
            getSelectedCharacterId: () => selectedCharacterId,
            publishCharacter: (character) => {
                const index = database.characters.findIndex(
                    (candidate) => candidate.chaId === character.chaId,
                )
                database.characters[index] = character
                residency.markCharacterHydrated(character.chaId)
                selectedCharacterId = character.chaId
            },
            publishConversation: vi.fn(),
            releaseInactiveCharacter: (id) => {
                residency.releaseCharacterToCatalog(database, id)
            },
        })

        expect(await workingSet.activateCharacter('char-b')).toBe(true)
        expect(await workingSet.activateCharacter('char-c')).toBe(true)

        expect(database.characters.map(isCatalogCharacterStub)).toEqual([true, true, false])
        expect(database.characters[0]).not.toHaveProperty('personality')
        expect(database.characters[1]).not.toHaveProperty('personality')
        expect(database.characters[2]).toHaveProperty('personality', 'body-c')
    })

    it('hydrates conversations concurrently while preserving configured order', async () => {
        const chats = ['chat-a', 'chat-b', 'chat-c', 'chat-d', 'chat-e'].map(makeChat)
        const harness = await makeHarness({
            characters: [makeCharacter('char-a', chats)],
            hydrateFullCharacter: true,
        })
        const pending = new Map<string, ReturnType<typeof deferred<void>>>()
        harness.storage.observe('readConversation', (_characterId, conversationId) => {
            const entry = deferred<void>()
            pending.set(conversationId, entry)
            return entry.promise
        })

        const activation = harness.workingSet.activateCharacter('char-a')
        await vi.waitFor(() => expect(pending.size).toBe(chats.length))
        for (const chat of [...chats].reverse()) pending.get(chat.id!)!.resolve()

        expect(await activation).toBe(true)
        expect(harness.publishedCharacters[0].chats.map((chat) => chat.id)).toEqual([
            'chat-a',
            'chat-b',
            'chat-c',
            'chat-d',
            'chat-e',
        ])
    })

    it('publishes only the newest rapid character navigation', async () => {
        const harness = await makeHarness({
            characters: [makeCharacter('char-a'), makeCharacter('char-b')],
        })
        const a = harness.storage.gate('readCharacter')

        const first = harness.workingSet.activateCharacter('char-a')
        await a.entered
        const second = harness.workingSet.activateCharacter('char-b')
        await second
        a.release()

        expect(await first).toBe(false)
        expect(harness.publishedCharacters.map((characterValue) => characterValue.chaId)).toEqual([
            'char-b',
        ])
        expect(harness.acquireRevision).not.toHaveBeenCalled()
    })

    it('does not publish navigation invalidated by activated database adoption', async () => {
        const harness = await makeHarness()
        const detail = harness.storage.gate('readCharacter')

        const activation = harness.workingSet.activateCharacter('char-a')
        await detail.entered
        harness.workingSet.invalidateNavigation()
        detail.release()

        expect(await activation).toBe(false)
        expect(harness.publishedCharacters).toEqual([])
    })

    it('does not replace from stale character preparation after newer navigation starts', async () => {
        const harness = await makeHarness({
            characters: [makeCharacter('char-a'), makeCharacter('char-b')],
        })
        const preparation = deferred<{ database: Database; reason: string } | null>()
        const prepare = vi.fn(() => preparation.promise)

        const first = harness.workingSet.activateCharacter('char-a', { prepare })
        await vi.waitFor(() => expect(prepare).toHaveBeenCalledOnce())
        const second = harness.workingSet.activateCharacter('char-b')
        expect(await second).toBe(true)
        preparation.resolve({ database: harness.database, reason: 'character-detail-replace' })

        expect(await first).toBe(false)
        expect(harness.coordinator.replacePersistentDatabase).not.toHaveBeenCalled()
        expect(harness.publishedCharacters.map((characterValue) => characterValue.chaId)).toEqual([
            'char-b',
        ])
    })

    it('preserves selection when character preparation produces no candidate', async () => {
        const harness = await makeHarness()

        expect(await harness.workingSet.activateCharacter('char-a', {
            prepare: async () => null,
        })).toBe(false)

        expect(harness.readCharacter).not.toHaveBeenCalled()
        expect(harness.coordinator.replacePersistentDatabase).not.toHaveBeenCalled()
        expect(harness.publishedCharacters).toEqual([])
    })

    it('does not publish an older character when newer navigation starts during replacement', async () => {
        const harness = await makeHarness({
            characters: [makeCharacter('char-a'), makeCharacter('char-b')],
        })
        const replacement = deferred<void>()
        let replacementStarted = false
        harness.coordinator.replacePersistentDatabase.mockImplementation(async () => {
            replacementStarted = true
            await replacement.promise
            return { kind: 'committed', revision: harness.revision, projection: 'applied' }
        })
        harness.coordinator.flushPendingData.mockImplementation(() =>
            replacementStarted ? replacement.promise : Promise.resolve(),
        )

        const first = harness.workingSet.activateCharacter('char-a', {
            prepare: async () => ({
                database: harness.database,
                reason: 'character-detail-replace',
            }),
        })
        await vi.waitFor(() =>
            expect(harness.coordinator.replacePersistentDatabase).toHaveBeenCalledOnce(),
        )
        const second = harness.workingSet.activateCharacter('char-b')
        replacement.resolve()

        expect(await first).toBe(false)
        expect(await second).toBe(true)
        expect(harness.publishedCharacters.map((characterValue) => characterValue.chaId)).toEqual([
            'char-b',
        ])
    })

    it('preserves the previous selection when hydration fails', async () => {
        const harness = await makeHarness({ characters: [] })

        await expect(harness.workingSet.activateCharacter('char-a')).rejects.toThrow(
            'Character char-a was not found',
        )

        expect(harness.publishedCharacters).toEqual([])
        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
        expect(harness.acquireRevision).not.toHaveBeenCalled()
    })

    it('does not release the current character when exact hydration is cancelled', async () => {
        const harness = await makeHarness()
        const detail = harness.storage.gate('readCharacter')

        const activation = harness.workingSet.activateCharacter('char-a')
        await detail.entered
        harness.workingSet.invalidateNavigation()
        detail.release()

        expect(await activation).toBe(false)
        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
    })

    it('rejects conversation navigation when the selected character changes during flush', async () => {
        const harness = await makeHarness({
            characters: [makeCharacter('char-a', [makeChat('chat-a')])],
        })
        harness.setSelectedCharacterId('char-a')
        const pendingFlush = deferred<void>()
        harness.coordinator.flushPendingData.mockReturnValueOnce(pendingFlush.promise)

        const activation = harness.workingSet.activateConversation('chat-a')
        harness.setSelectedCharacterId('char-b')
        harness.database.characters.reverse()
        pendingFlush.resolve()
        expect(await activation).toBe(false)

        expect(harness.readConversation).not.toHaveBeenCalled()
        expect(harness.publishedConversations).toEqual([])
        expect(harness.coordinator.adoptHydratedCharacter).not.toHaveBeenCalled()
        expect(harness.acquireRevision).not.toHaveBeenCalled()
    })

    it('coalesces duplicate stable-ID conversation hydration into one flight', async () => {
        const harness = await makeHarness({
            characters: [makeCharacter('char-a', [makeChat('chat-a')])],
        })
        const conversationRead = harness.storage.gate('readConversation')
        harness.setSelectedCharacterId('char-a')

        const first = harness.workingSet.activateConversation('chat-a')
        const second = harness.workingSet.activateConversation('chat-a')
        await conversationRead.entered
        conversationRead.release()

        await expect(Promise.all([first, second])).resolves.toEqual([true, true])
        expect(harness.readConversation).toHaveBeenCalledOnce()
        expect(harness.publishedConversations).toHaveLength(1)
    })

    it('starts a fresh conversation flight when a later navigation returns to the same ID', async () => {
        const firstB = deferred<{ revision: number; value: Chat } | null>()
        const chatC = deferred<{ revision: number; value: Chat } | null>()
        const latestB = deferred<{ revision: number; value: Chat } | null>()
        const bReads = [firstB, latestB]
        const harness = await makeHarness()
        // Each read of chat-b answers with its own body, so the published one shows which flight won.
        const readConversation = harness.readConversation.mockImplementation(
            (_characterId, conversationId) => {
                if (conversationId === 'chat-c') return chatC.promise
                return bReads.shift()!.promise
            },
        )
        harness.setSelectedCharacterId('char-a')

        const staleBActivation = harness.workingSet.activateConversation('chat-b')
        await vi.waitFor(() => expect(readConversation).toHaveBeenCalledTimes(1))
        const staleCActivation = harness.workingSet.activateConversation('chat-c')
        await vi.waitFor(() => expect(readConversation).toHaveBeenCalledTimes(2))
        const latestBActivation = harness.workingSet.activateConversation('chat-b')

        await vi.waitFor(() => expect(readConversation).toHaveBeenCalledTimes(3))
        firstB.resolve({ revision: harness.revision, value: {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'stale B' }],
        } as Chat })
        chatC.resolve({ revision: harness.revision, value: makeChat('chat-c') })
        latestB.resolve({ revision: harness.revision, value: {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'latest B' }],
        } as Chat })

        await expect(Promise.all([
            staleBActivation,
            staleCActivation,
            latestBActivation,
        ])).resolves.toEqual([false, false, true])
        expect(harness.publishedConversations).toEqual([{
            characterId: 'char-a',
            conversation: expect.objectContaining({
                id: 'chat-b',
                message: [{ role: 'char', data: 'latest B' }],
            }),
        }])
    })

    it('flushes the previous body before publishing its summary stub', async () => {
        const chatA = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'dirty body' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'authoritative body' }],
        } as Chat
        let resident = makeCharacter('char-a', [chatA, makeChat('chat-b')])
        resident.chatPage = 0
        const flush = deferred<void>()
        const events: string[] = []
        const storage = await createPersistentStoreHarness({
            username: 'Fixture',
            botPresets: [],
            characters: [makeCharacter('char-a', [chatA, chatB])],
        } as unknown as Database)
        storage.observe('readConversation', () => {
            events.push('read')
        })
        const coordinator = {
            revision: storage.revision,
            mutationGeneration: 4,
            initialize: vi.fn(),
            flushPendingData: vi.fn(async () => {
                events.push('flush:start')
                await flush.promise
                events.push('flush:done')
            }),
            replacePersistentDatabase: vi.fn(async () => undefined),
            adoptHydratedCharacter: vi.fn(() => {
                events.push('baseline')
                return true
            }),
            markPersistentDataDirty: vi.fn(),
        }
        const workingSet = new ActiveWorkingSet({
            store: storage.store,
            coordinator,
            getSelectedCharacterId: () => 'char-a',
            getResidentCharacter: () => resident,
            canReleaseConversation: () => true,
            publishCharacter: vi.fn(),
            publishConversation: (_characterId, _conversation, nextCharacter) => {
                events.push('publish')
                resident = nextCharacter as character
            },
        })

        const activation = workingSet.activateConversation('chat-b')
        await vi.waitFor(() => expect(coordinator.flushPendingData).toHaveBeenCalledOnce())
        expect(resident.chats[0].message).toEqual(chatA.message)
        flush.resolve()

        await expect(activation).resolves.toBe(true)
        expect(events).toEqual(['flush:start', 'flush:done', 'read', 'baseline', 'publish'])
        expect(resident.chatPage).toBe(1)
        expect(resident.chats).toMatchObject([
            { id: 'chat-a', message: [] },
            { id: 'chat-b', message: chatB.message },
        ])
    })

    it('publishes a full-array session for the live conversation after navigation', async () => {
        const chatA = makeChat('chat-a')
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'user', data: 'authoritative body', chatId: 'message-b' }],
        } as Chat
        let resident = makeCharacter('char-a', [chatA, makeChat('chat-b')])
        resident.chatPage = 0
        const storage = await createPersistentStoreHarness({
            username: 'Fixture',
            botPresets: [],
            characters: [makeCharacter('char-a', [chatA, chatB])],
        } as unknown as Database)
        const coordinator = {
            revision: storage.revision,
            mutationGeneration: 0,
            initialize: vi.fn(),
            flushPendingData: vi.fn(async () => undefined),
            replacePersistentDatabase: vi.fn(async () => undefined),
            adoptHydratedCharacter: vi.fn(() => true),
            markPersistentDataDirty: vi.fn(),
            recordActiveConversationMutation: vi.fn(),
        }
        const workingSet = new ActiveWorkingSet({
            store: storage.store,
            coordinator,
            getSelectedCharacterId: () => 'char-a',
            getResidentCharacter: () => resident,
            publishCharacter: vi.fn(),
            publishConversation: (_characterId, _conversation, nextCharacter) => {
                resident = nextCharacter as character
            },
        })

        await expect(workingSet.activateConversation('chat-b')).resolves.toBe(true)

        const session = workingSet.activeConversationSession!
        expect(session.storeRevision).toBe(storage.revision)
        expect(session.materializeCompatibilityArray()).toBe(resident.chats[1].message)
        const appended = session.append({
            role: 'char',
            data: 'session append',
            chatId: 'session-append',
        })
        expect(coordinator.recordActiveConversationMutation).toHaveBeenCalledWith(
            expect.objectContaining({
                characterId: 'char-a',
                conversationId: 'chat-b',
                sessionVersion: 1,
            }),
        )
        const mutation = coordinator.recordActiveConversationMutation.mock.calls[0][0]
        const persistence = workingSet.beginConversationMutationPersistence(mutation)
        expect(persistence).not.toBeNull()
        expect(session.pinCount('pending-save')).toBe(1)
        expect(workingSet.acknowledgeConversationMutationPersisted({
            characterId: 'char-a',
            conversationId: 'chat-b',
            sessionToken: mutation.sessionToken,
            sessionVersion: 1,
            revision: storage.revision + 1,
        })).toBe(true)
        persistence!.release()
        expect(session.pinCount('pending-save')).toBe(0)
        expect(session.persistedVersion).toBe(1)
        const edited = session.edit(appended, {
            role: 'char',
            data: 'session edit',
            chatId: 'session-append',
        })
        expect(resident.chats[1].message.at(-1)?.data).toBe('session edit')
        session.delete(edited)
        expect(resident.chats[1].message).toEqual(chatB.message)

        await expect(workingSet.activateConversation('chat-b')).resolves.toBe(true)
        expect(workingSet.activeConversationSession).not.toBe(session)
        expect(session.isActive).toBe(false)
        expect(() => session.append({ role: 'char', data: 'detached navigation write' })).toThrow(
            /inactive/,
        )

        const releasedSession = workingSet.activeConversationSession!
        await expect(workingSet.deactivate()).resolves.toBe(true)
        expect(releasedSession.isActive).toBe(false)
        expect(() => releasedSession.append({ role: 'char', data: 'detached release write' })).toThrow(
            /inactive/,
        )
    })

    it('keeps the session command body resident when its pending save fails before eviction', async () => {
        const previous = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'unsaved' }],
        } as Chat
        const resident = makeCharacter('char-a', [previous, makeChat('chat-b')])
        resident.chatPage = 0
        const database = {
            username: 'Fixture',
            characters: [resident],
        } as unknown as Database
        const storage = await createPersistentStoreHarness({
            username: 'Fixture',
            botPresets: [],
            characters: [makeCharacter('char-a', [makeChat('chat-a'), makeChat('chat-b')])],
        } as unknown as Database)
        const readConversation = storage.spy('readConversation')
        const publishConversation = vi.fn()
        const coordinator = {
            revision: storage.revision,
            mutationGeneration: 1,
            initialize: vi.fn(),
            flushPendingData: vi.fn(async () => { throw new Error('commit failed') }),
            replacePersistentDatabase: vi.fn(async () => undefined),
            adoptHydratedCharacter: vi.fn(() => true),
            markPersistentDataDirty: vi.fn(),
            recordActiveConversationMutation: vi.fn(),
        }
        const workingSet = new ActiveWorkingSet({
            store: storage.store,
            coordinator,
            getSelectedCharacterId: () => 'char-a',
            getResidentCharacter: () => resident,
            canReleaseConversation: () => true,
            publishCharacter: vi.fn(),
            publishConversation,
        })
        workingSet.installCommittedWorkingSet(database, storage.revision)
        const session = workingSet.activeConversationSession!
        session.append({ role: 'char', data: 'pending command' })

        await expect(workingSet.activateConversation('chat-b')).rejects.toThrow('commit failed')
        expect(coordinator.recordActiveConversationMutation).toHaveBeenCalledOnce()
        expect(session.persistedVersion).toBe(0)
        expect(workingSet.activeConversationSession).toBe(session)
        expect(readConversation).not.toHaveBeenCalled()
        expect(publishConversation).not.toHaveBeenCalled()
        expect(resident.chats[0]).toBe(previous)
        expect(resident.chats[0].message).toEqual([
            { role: 'user', data: 'unsaved' },
            { role: 'char', data: 'pending command' },
        ])
        const currentTarget = workingSet.captureSelectedConversationTarget()
        expect(currentTarget).not.toBeNull()
        const currentLease = await workingSet.acquireCompleteConversation(
            'after-failed-activation',
            currentTarget!,
        )
        currentLease.release()
    })

    it('keeps a streaming previous body pinned while selecting the hydrated target', async () => {
        const streaming = {
            ...makeChat('chat-a'),
            isStreaming: true,
            message: [{ role: 'char', data: 'partial stream' }],
        } as Chat
        const target = {
            ...makeChat('chat-b'),
            message: [{ role: 'user', data: 'target' }],
        } as Chat
        let resident = makeCharacter('char-a', [streaming, makeChat('chat-b')])
        resident.chatPage = 0
        const storage = await createPersistentStoreHarness({
            username: 'Fixture',
            botPresets: [],
            characters: [makeCharacter('char-a', [makeChat('chat-a'), target])],
        } as unknown as Database)
        const workingSet = new ActiveWorkingSet({
            store: storage.store,
            coordinator: {
                revision: storage.revision,
                mutationGeneration: 0,
                initialize: vi.fn(),
                flushPendingData: vi.fn(async () => undefined),
                replacePersistentDatabase: vi.fn(async () => undefined),
                adoptHydratedCharacter: vi.fn(() => true),
                markPersistentDataDirty: vi.fn(),
            },
            getSelectedCharacterId: () => 'char-a',
            getResidentCharacter: () => resident,
            canReleaseConversation: (character, conversationId) =>
                !character.chats.find((conversation) => conversation.id === conversationId)
                    ?.isStreaming,
            publishCharacter: vi.fn(),
            publishConversation: (_characterId, _conversation, nextCharacter) => {
                resident = nextCharacter as character
            },
        })

        await expect(workingSet.activateConversation('chat-b')).resolves.toBe(true)
        expect(resident.chats[0]).toBe(streaming)
        expect(resident.chats[0].message).toEqual([
            { role: 'char', data: 'partial stream' },
        ])
        expect(resident.chats[1]).toEqual(target)
    })

    it('keeps the committed previous body authoritative after a later selected save', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            'conversation-residency-commit-before-release',
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const chatA = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'first' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'second' }],
        } as Chat
        const chatC = {
            ...makeChat('chat-c'),
            fmIndex: 2,
            scriptstate: { source: 'authoritative' },
            note: 'authoritative note',
            localLore: [{ key: 'lore', content: 'authoritative lore' }],
            message: [{ role: 'user', data: 'must survive structural save' }],
        } as unknown as Chat
        let database = {
            username: 'Fixture',
            botPresets: [],
            characters: [makeCharacter('char-a', [chatA, chatB, chatC])],
        } as unknown as Database
        database.characters[0].chatPage = 0
        const imported = await store.replaceFromDatabase(database)
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                getSelectedCharacterId: () => 'char-a',
                getSelectedConversationId: () =>
                    database.characters[0].chats[database.characters[0].chatPage].id,
                replaceDatabase: (value) => { database = value },
                publishCharacter: (value) => { database.characters[0] = value },
                publishConversation: (_characterId, conversation, nextCharacter) => {
                    if (nextCharacter) {
                        database.characters[0] = nextCharacter
                        return
                    }
                    const character = database.characters[0]
                    const index = character.chats.findIndex((chat) => chat.id === conversation.id)
                    character.chats[index] = conversation
                    character.chatPage = index
                },
                shouldHydrateFullCharacter: () => false,
                canReleaseConversation: () => true,
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)
        expect(runtime.revision).toBe(imported.revision)
        expect(await runtime.activateCharacter('char-a')).toBe(true)

        database.characters[0].chats[0].message.push({ role: 'char', data: 'saved before release' })
        runtime.markPersistentDataDirty(64)
        expect(await runtime.activateConversation('chat-b')).toBe(true)
        expect(runtime.getActiveConversationSession()).toMatchObject({
            characterId: 'char-a',
            conversationId: 'chat-b',
            storeRevision: imported.revision + 1,
        })
        expect(database.characters[0].chats[0].message).toEqual([])
        expect((await store.readConversation('char-a', 'chat-a'))?.value.message).toEqual([
            { role: 'user', data: 'first' },
            { role: 'char', data: 'saved before release' },
        ])

        const activeSession = runtime.getActiveConversationSession()!
        activeSession.append({ role: 'user', data: 'later save' })
        await runtime.flushPendingData('conversation-residency-test')
        expect(activeSession.persistedVersion).toBe(1)
        expect(activeSession.storeRevision).toBe(imported.revision + 2)

        expect((await store.readConversation('char-a', 'chat-a'))?.value.message).toEqual([
            { role: 'user', data: 'first' },
            { role: 'char', data: 'saved before release' },
        ])
        expect((await store.readConversation('char-a', 'chat-b'))?.value.message).toEqual([
            { role: 'char', data: 'second' },
            { role: 'user', data: 'later save' },
        ])

        database.characters[0].chats[2].name = 'Renamed summary'
        database.characters[0].chats[2].fmIndex = -1
        database.characters[0].chats[2].scriptstate = { source: 'synthetic' }
        runtime.markPersistentDataDirty(16)
        await runtime.flushPendingData('conversation-summary-metadata-test')
        expect(await store.readConversation('char-a', 'chat-c')).toMatchObject({
            value: {
                name: 'Renamed summary',
                fmIndex: 2,
                scriptstate: { source: 'authoritative' },
                note: 'authoritative note',
                localLore: [{ key: 'lore', content: 'authoritative lore' }],
                message: [{ role: 'user', data: 'must survive structural save' }],
            },
        })

        database.characters[0].chats.splice(0, 1)
        database.characters[0].chatPage = 0
        runtime.markPersistentDataDirty(16)
        await runtime.flushPendingData('conversation-structure-test')

        expect((await store.readConversation('char-a', 'chat-c'))?.value).toMatchObject({
            fmIndex: 2,
            scriptstate: { source: 'authoritative' },
            message: [{ role: 'user', data: 'must survive structural save' }],
        })
    })

    it('persists navigation and selected edits without rebuilding inactive conversations', async () => {
        const indexedDB = new IDBFactory()
        const store = new IndexedDbPersistentDataStore(
            'conversation-selection-after-navigation',
            indexedDB,
            IDBKeyRange,
        )
        await store.open()
        const readConversation = vi.spyOn(store, 'readConversation')
        const chatA = {
            ...makeChat('chat-a'),
            message: [{ role: 'user', data: 'first body' }],
        } as Chat
        const chatB = {
            ...makeChat('chat-b'),
            message: [{ role: 'char', data: 'second body' }],
        } as Chat
        let database = {
            username: 'Fixture',
            botPresets: [],
            characters: [makeCharacter('char-a', [chatA, chatB])],
        } as unknown as Database
        database.characters[0].chatPage = 0
        await store.replaceFromDatabase(database)
        const commit = vi.spyOn(store, 'commit')
        const runtime = createPersistentDataRuntime({
            store,
            state: {
                captureRoot: () => capturePersistentRoot(database),
                capturePresets: () => database.botPresets,
                captureSelectedCharacter: () => database.characters[0],
                captureCharacter: (id) =>
                    database.characters.find((character) => character.chaId === id) ?? null,
                getSelectedCharacterId: () => 'char-a',
                getSelectedConversationId: () =>
                    database.characters[0].chats[database.characters[0].chatPage].id,
                replaceDatabase: (value) => { database = value },
                publishCharacter: (value) => { database.characters[0] = value },
                publishConversation: (_characterId, conversation, nextCharacter) => {
                    if (nextCharacter) {
                        database.characters[0] = nextCharacter
                        return
                    }
                    const character = database.characters[0]
                    const index = character.chats.findIndex((chat) => chat.id === conversation.id)
                    character.chats[index] = conversation
                    character.chatPage = index
                },
                shouldHydrateFullCharacter: () => false,
                canReleaseConversation: () => true,
            },
            prepareDatabase: async (value) => value,
        })
        await runtime.initializeActiveWorkingSet(database)
        expect(await runtime.activateCharacter('char-a')).toBe(true)
        expect(await runtime.activateConversation('chat-b')).toBe(true)
        readConversation.mockClear()

        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingData('conversation-selection-test')

        expect(readConversation).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0]).toMatchObject({
            character: { chaId: 'char-a', chatPage: 1 },
        })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('conversations')
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect((await store.readCharacter('char-a'))?.value.chatPage).toBe(1)
        expect((await store.readConversation('char-a', 'chat-a'))?.value.message).toEqual([
            { role: 'user', data: 'first body' },
        ])
        expect((await store.readConversation('char-a', 'chat-b'))?.value.message).toEqual([
            { role: 'char', data: 'second body' },
        ])

        commit.mockClear()
        expect(await runtime.activateConversation('chat-a')).toBe(true)
        readConversation.mockClear()
        runtime.getActiveConversationSession()!.append({
            role: 'char',
            data: 'selected edit',
        })
        runtime.markPersistentDataDirty(1)
        await runtime.flushPendingData('conversation-selection-and-edit-test')

        expect(readConversation).not.toHaveBeenCalled()
        expect(commit).toHaveBeenCalledOnce()
        expect(commit.mock.calls[0][0]).toMatchObject({
            character: { chaId: 'char-a', chatPage: 0 },
            conversations: [{
                type: 'replace-range',
                characterId: 'char-a',
                conversationId: 'chat-a',
                start: 1,
                deleteCount: 0,
                messages: [{ role: 'char', data: 'selected edit' }],
            }],
        })
        expect(commit.mock.calls[0][0]).not.toHaveProperty('replaceCharacter')
        expect((await store.readCharacter('char-a'))?.value.chatPage).toBe(0)
        expect((await store.readConversation('char-a', 'chat-a'))?.value.message).toEqual([
            { role: 'user', data: 'first body' },
            { role: 'char', data: 'selected edit' },
        ])
        expect((await store.readConversation('char-a', 'chat-b'))?.value.message).toEqual([
            { role: 'char', data: 'second body' },
        ])
    })

    it('discards hydration when the coordinator revision changes', async () => {
        const harness = await makeHarness()
        harness.coordinator.revision = harness.revision + 1

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(false)
        expect(harness.publishedCharacters).toEqual([])
        expect(harness.acquireRevision).not.toHaveBeenCalled()
    })

    it('discards a result when the revision changes during hydration', async () => {
        const harness = await makeHarness()
        const detail = harness.storage.gate('readCharacter')

        const activation = harness.workingSet.activateCharacter('char-a')
        await detail.entered
        harness.coordinator.revision++
        detail.release()

        expect(await activation).toBe(false)
        expect(harness.publishedCharacters).toEqual([])
        expect(harness.acquireRevision).not.toHaveBeenCalled()
    })

    it('does not publish a stale conversation after the resident chat changes', async () => {
        const harness = await makeHarness({
            characters: [makeCharacter('char-a', [makeChat('chat-a')])],
        })
        const conversationRead = harness.storage.gate('readConversation')
        harness.setSelectedCharacterId('char-a')
        const previous = {
            ...makeChat('chat-old'),
            message: [{ role: 'user', data: 'must remain resident' }],
        } as Chat
        const resident = makeCharacter('char-a', [previous, makeChat('chat-a')])
        resident.chatPage = 0
        harness.database.characters = [resident]

        const activation = harness.workingSet.activateConversation('chat-a')
        await conversationRead.entered
        harness.coordinator.mutationGeneration++
        conversationRead.release()

        expect(await activation).toBe(false)
        expect(harness.publishedConversations).toEqual([])
        expect(harness.database.characters[0].chats[0]).toBe(previous)
        expect(harness.database.characters[0].chats[0].message).toEqual([
            { role: 'user', data: 'must remain resident' },
        ])
    })

    it('keeps the previous body resident when the selected character changes during hydration', async () => {
        const harness = await makeHarness({
            characters: [makeCharacter('char-a', [makeChat('chat-a')])],
        })
        const conversationRead = harness.storage.gate('readConversation')
        harness.setSelectedCharacterId('char-a')
        const previous = {
            ...makeChat('chat-old'),
            message: [{ role: 'user', data: 'must remain resident' }],
        } as Chat
        const resident = makeCharacter('char-a', [previous, makeChat('chat-a')])
        resident.chatPage = 0
        harness.database.characters = [resident]

        const activation = harness.workingSet.activateConversation('chat-a')
        await conversationRead.entered
        harness.setSelectedCharacterId('char-b')
        conversationRead.release()

        expect(await activation).toBe(false)
        expect(harness.publishedConversations).toEqual([])
        expect(harness.database.characters[0].chats[0]).toBe(previous)
    })

    it('does not adopt a hydrated body after the resident working set changes', async () => {
        const harness = await makeHarness()
        const detail = harness.storage.gate('readCharacter')

        const activation = harness.workingSet.activateCharacter('char-a')
        await detail.entered
        harness.coordinator.mutationGeneration++
        detail.release()

        expect(await activation).toBe(false)
        expect(harness.coordinator.adoptHydratedCharacter).not.toHaveBeenCalled()
        expect(harness.publishedCharacters).toEqual([])
        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
    })

    it('does not publish or release when generation starts during character hydration', async () => {
        const harness = await makeHarness()
        const detail = harness.storage.gate('readCharacter')

        const activation = harness.workingSet.activateCharacter('char-a')
        await detail.entered
        harness.setWorkingSetActivationAllowed(false)
        detail.release()

        expect(await activation).toBe(false)
        expect(harness.coordinator.adoptHydratedCharacter).not.toHaveBeenCalled()
        expect(harness.publishCharacter).not.toHaveBeenCalled()
        expect(harness.releaseInactiveCharacter).not.toHaveBeenCalled()
    })

    it('does not publish when the hydrated baseline can no longer be adopted', async () => {
        const harness = await makeHarness()
        harness.coordinator.adoptHydratedCharacter.mockReturnValueOnce(false)

        expect(await harness.workingSet.activateCharacter('char-a')).toBe(false)

        expect(harness.publishedCharacters).toEqual([])
        expect(harness.acquireRevision).not.toHaveBeenCalled()
    })

    it('opens the store revision and initializes coordinator baselines', async () => {
        const harness = await makeHarness()

        await harness.workingSet.initializeActiveWorkingSet(harness.database)

        expect(harness.open).toHaveBeenCalledTimes(1)
        expect(harness.coordinator.initialize).toHaveBeenCalledWith(harness.revision, harness.database)
    })

    it('rehydrates after reopen through bounded reads without writing IndexedDB', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'active-working-set-reopen'
        const initial = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await initial.open()
        const imported = await initial.replaceFromDatabase(fixtureDatabase)
        const reopened = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reopened.open()
        const acquireRevision = vi.spyOn(reopened, 'acquireRevision')
        const writes = { put: 0, add: 0, delete: 0, clear: 0 }
        const originalPut = IDBObjectStore.prototype.put
        const originalAdd = IDBObjectStore.prototype.add
        const originalDelete = IDBObjectStore.prototype.delete
        const originalClear = IDBObjectStore.prototype.clear
        const putSpy = vi.spyOn(IDBObjectStore.prototype, 'put').mockImplementation(function (...args) {
            writes.put++
            return originalPut.apply(this, args as Parameters<IDBObjectStore['put']>)
        })
        const addSpy = vi.spyOn(IDBObjectStore.prototype, 'add').mockImplementation(function (...args) {
            writes.add++
            return originalAdd.apply(this, args as Parameters<IDBObjectStore['add']>)
        })
        const deleteSpy = vi.spyOn(IDBObjectStore.prototype, 'delete').mockImplementation(function (...args) {
            writes.delete++
            return originalDelete.apply(this, args as Parameters<IDBObjectStore['delete']>)
        })
        const clearSpy = vi.spyOn(IDBObjectStore.prototype, 'clear').mockImplementation(function () {
            writes.clear++
            return originalClear.apply(this)
        })
        const published: Array<character> = []
        const coordinator = {
            revision: imported.revision,
            mutationGeneration: 0,
            initialize: vi.fn(),
            flushPendingData: vi.fn(async () => undefined),
            replacePersistentDatabase: vi.fn(async () => undefined),
            adoptHydratedCharacter: vi.fn(() => true),
            markPersistentDataDirty: vi.fn(),
        }
        const workingSet = new ActiveWorkingSet({
            store: reopened,
            coordinator,
            getSelectedCharacterId: () => 'char-a',
            publishCharacter: (value) => published.push(value),
            publishConversation: vi.fn(),
        })

        try {
            expect(await workingSet.activateCharacter('char-a')).toBe(true)
        } finally {
            putSpy.mockRestore()
            addSpy.mockRestore()
            deleteSpy.mockRestore()
            clearSpy.mockRestore()
        }

        expect(published[0]).toEqual({
            ...fixtureDatabase.characters[1],
            chats: [
                fixtureDatabase.characters[1].chats[0],
                { ...fixtureDatabase.characters[1].chats[1], message: [] },
            ],
        })
        expect(acquireRevision).not.toHaveBeenCalled()
        expect(writes).toEqual({ put: 0, add: 0, delete: 0, clear: 0 })
    })

    it('discards mixed revisions when an intervening commit makes the next page empty', async () => {
        const indexedDB = new IDBFactory()
        const databaseName = 'active-working-set-revision-race'
        const reader = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        const writer = new IndexedDbPersistentDataStore(databaseName, indexedDB, IDBKeyRange)
        await reader.open()
        const imported = await reader.replaceFromDatabase(fixtureDatabase)
        await writer.open()
        const originalQuery = reader.queryConversations.bind(reader)
        const originalReadConversation = reader.readConversation.bind(reader)
        let firstPage = true
        vi.spyOn(reader, 'queryConversations').mockImplementation(async (input) => {
            const page = await originalQuery(input)
            if (firstPage) {
                firstPage = false
                return { ...page, nextCursor: '1' }
            }
            return page
        })
        let firstConversation = true
        vi.spyOn(reader, 'readConversation').mockImplementation(async (characterId, conversationId) => {
            const conversation = await originalReadConversation(characterId, conversationId)
            if (firstConversation) {
                firstConversation = false
                const root = await writer.readRoot()
                await writer.commit({
                    expectedRevision: root.revision,
                    root: { ...root.value, username: 'Intervening commit' },
                })
            }
            return conversation
        })
        const publishCharacter = vi.fn()
        const workingSet = new ActiveWorkingSet({
            store: reader,
            coordinator: {
                revision: imported.revision,
                mutationGeneration: 0,
                initialize: vi.fn(),
                flushPendingData: vi.fn(async () => undefined),
                replacePersistentDatabase: vi.fn(async () => undefined),
                adoptHydratedCharacter: vi.fn(() => true),
                markPersistentDataDirty: vi.fn(),
            },
            getSelectedCharacterId: () => 'char-b',
            publishCharacter,
            publishConversation: vi.fn(),
        })

        expect(await workingSet.activateCharacter('char-b')).toBe(false)
        expect(publishCharacter).not.toHaveBeenCalled()
        expect(vi.mocked(reader.queryConversations)).toHaveBeenCalledTimes(2)
        expect((await originalQuery({ characterId: 'char-b', order: 'configured', limit: 100, cursor: '1' })).items).toEqual([])
    })
})
