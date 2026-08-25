import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { character, groupChat } from '../storage/database.svelte'

const mocks = vi.hoisted(() => ({
    nextId: 0,
}))

vi.mock('uuid', () => ({
    v4: () => `generated-${++mocks.nextId}`,
}))
vi.mock('../alert', async () => {
    const { writable } = await import('svelte/store')
    return {
        alertError: vi.fn(),
        alertInput: vi.fn(),
        alertNormal: vi.fn(),
        alertStore: writable({ type: 'none', msg: '' }),
        alertWait: vi.fn(),
    }
})
vi.mock('../storage/database.svelte', () => ({
    setDatabase: vi.fn(),
    saveImage: vi.fn(),
    getCurrentChat: vi.fn(),
    setCurrentChat: vi.fn(),
    getDatabase: vi.fn(),
    normalizeDatabaseDefaults: vi.fn(),
    defaultSdDataFunc: () => ({}),
}))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    activateCharacter: vi.fn(),
    invalidatePersistentNavigation: vi.fn(),
    upsertPersistentCompleteCharacter: vi.fn(),
}))
vi.mock('../stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return { selectedCharID: writable(-1) }
})
vi.mock('../util', () => ({ sleep: vi.fn() }))
vi.mock('../globalApi.svelte', () => ({ readImage: vi.fn() }))
vi.mock('../process/index.svelte', async () => {
    const { writable } = await import('svelte/store')
    return { doingChat: writable(false) }
})

import {
    createMultiuserReceiveController,
    normalizeIncomingCharacter,
    normalizeIncomingChat,
    normalizeIncomingChatForCurrent,
    type MultiuserReceiveControllerDependencies,
} from './multiuser'

type CompleteCharacter = character | groupChat

function deferred<T = void>() {
    let resolve!: (value: T | PromiseLike<T>) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

const buildCharacter = (overrides: Record<string, unknown> = {}) => ({
    chaId: '§temp',
    name: 'Host',
    chats: [{
        message: [],
        note: '',
        name: 'Host Chat',
        localLore: [],
        id: 'host-chat',
    }],
    chatPage: 0,
    ...overrides,
}) as CompleteCharacter

const buildIncoming = (overrides: Record<string, unknown> = {}) => ({
    chaId: 'host-char',
    name: 'Host',
    chats: [{ message: [], note: '', name: 'Host Chat', localLore: [] }],
    chatPage: 3,
    ...overrides,
}) as any

function createStatefulDependencies(initial: CompleteCharacter | null = null) {
    let temp = initial ? structuredClone(initial) : null
    const upsert: MultiuserReceiveControllerDependencies['upsertCompleteCharacter'] = async (
        _id,
        _reason,
        createOrMutate,
    ) => {
        temp = structuredClone(await createOrMutate(temp ? structuredClone(temp) : null))
        return true
    }
    const dependencies: MultiuserReceiveControllerDependencies = {
        upsertCompleteCharacter: vi.fn(upsert),
        activateCharacter: vi.fn(async () => true),
        invalidateNavigation: vi.fn(),
        deselect: vi.fn(),
        onChatCommitted: vi.fn(),
    }
    return {
        dependencies,
        current: () => temp,
        upsert,
    }
}

describe('multiuser detached normalization', () => {
    beforeEach(() => {
        mocks.nextId = 0
        vi.clearAllMocks()
    })

    it('normalizes a detached copy without requiring a catalog database snapshot', () => {
        const incoming = buildIncoming({ chats: [undefined, {
            message: [],
            note: '',
            name: 'Second',
            localLore: [],
        }] })

        const normalized = normalizeIncomingCharacter(incoming)

        expect(normalized).not.toBe(incoming)
        expect(normalized.chaId).toBe('§temp')
        expect(normalized.chatPage).toBe(0)
        expect(normalized.chats).toHaveLength(1)
        expect(normalized.chats[0].id).toBe('generated-1')
        expect(incoming.chaId).toBe('host-char')
        expect(incoming.chatPage).toBe(3)
        expect(incoming.chats[0]).toBeUndefined()
    })

    it('assigns fresh local chat ids instead of trusting wire ids', () => {
        const normalized = normalizeIncomingCharacter(buildIncoming({
            chats: [
                { message: [], note: '', name: 'One', localLore: [], id: 'duplicate' },
                { message: [], note: '', name: 'Two', localLore: [], id: 'duplicate' },
            ],
        }))

        expect(normalized.chats[0].id).toBe('generated-1')
        expect(normalized.chats[1].id).toBe('generated-2')
    })

    it('cannot collide with an existing catalog character chat id from the wire payload', () => {
        const existingCatalogCharacter = {
            chaId: 'local-char',
            chats: [{ id: 'existing-library-chat' }],
        }
        const incoming = buildIncoming({
            chats: [{
                message: [],
                note: '',
                name: 'Host Chat',
                localLore: [],
                id: existingCatalogCharacter.chats[0].id,
            }],
        })

        const normalized = normalizeIncomingCharacter(incoming)

        expect(normalized.chats[0].id).toBe('generated-1')
        expect(normalized.chats[0].id).not.toBe(existingCatalogCharacter.chats[0].id)
        expect(incoming.chats[0].id).toBe(existingCatalogCharacter.chats[0].id)
    })
})

describe('multiuser ordered working-set receive controller', () => {
    beforeEach(() => {
        mocks.nextId = 0
        vi.clearAllMocks()
    })

    it('upserts only the temp character and preserves unrelated catalog entries', async () => {
        const unrelatedCatalogStub = { chaId: 'local-char', name: 'Local', chats: [] }
        const state = new Map<string, unknown>([['local-char', unrelatedCatalogStub]])
        const dependencies: MultiuserReceiveControllerDependencies = {
            upsertCompleteCharacter: vi.fn(async (id, _reason, createOrMutate) => {
                const current = state.get(id) as CompleteCharacter | undefined
                state.set(id, await createOrMutate(current ?? null))
                return true
            }),
            activateCharacter: vi.fn(async () => true),
            invalidateNavigation: vi.fn(),
            deselect: vi.fn(),
            onChatCommitted: vi.fn(),
        }
        const controller = createMultiuserReceiveController(dependencies)

        await controller.receiveCharacter(buildIncoming())
        await controller.receiveCharacter(buildIncoming({ name: 'Replacement' }))

        expect(state.get('local-char')).toBe(unrelatedCatalogStub)
        expect(state.size).toBe(2)
        expect((state.get('§temp') as CompleteCharacter).name).toBe('Replacement')
        expect(dependencies.upsertCompleteCharacter).toHaveBeenCalledWith(
            '§temp',
            'multiuser-receive-character',
            expect.any(Function),
            { includeInCharacterOrder: false },
        )
        expect(dependencies.activateCharacter).toHaveBeenCalledWith('§temp')
    })

    it('replaces a chat from the authoritative temp character instead of a live catalog stub', async () => {
        const liveCatalogStub = { chaId: '§temp', name: 'Host', chats: [] }
        let authoritative = buildCharacter({
            chats: [{
                message: [{ role: 'char', data: 'authoritative' }],
                note: '',
                name: 'Authoritative',
                localLore: [],
                id: 'authoritative-chat',
            }],
        })
        const dependencies: MultiuserReceiveControllerDependencies = {
            upsertCompleteCharacter: vi.fn(async (_id, _reason, createOrMutate) => {
                authoritative = await createOrMutate(structuredClone(authoritative))
                return true
            }),
            activateCharacter: vi.fn(async () => true),
            invalidateNavigation: vi.fn(),
            deselect: vi.fn(),
            onChatCommitted: vi.fn(),
        }
        const controller = createMultiuserReceiveController(dependencies)

        await controller.receiveCharacter(buildIncoming({
            chats: [{
                message: [{ role: 'char', data: 'authoritative' }],
                note: '',
                name: 'Authoritative',
                localLore: [],
                id: 'wire-character-chat',
            }],
        }))
        const localChatId = authoritative.chats[0].id
        await controller.receiveChat({
            message: [{ role: 'char', data: 'received' }],
            note: '',
            name: 'Received',
            localLore: [],
            id: 'wire-chat-id',
        } as any)

        expect(liveCatalogStub.chats).toEqual([])
        expect(authoritative.chats[0].id).toBe(localChatId)
        expect(authoritative.chats[0].id).not.toBe('wire-chat-id')
        expect(authoritative.chats[0].message[0].data).toBe('received')
    })

    it('serializes character installation before a following chat replacement', async () => {
        const gate = deferred()
        const events: string[] = []
        let temp: CompleteCharacter | null = null
        const dependencies: MultiuserReceiveControllerDependencies = {
            upsertCompleteCharacter: vi.fn(async (_id, reason, createOrMutate) => {
                events.push(`${reason}:start`)
                if (reason === 'multiuser-receive-character') await gate.promise
                temp = await createOrMutate(temp)
                events.push(`${reason}:end`)
                return true
            }),
            activateCharacter: vi.fn(async () => {
                events.push('activate')
                return true
            }),
            invalidateNavigation: vi.fn(),
            deselect: vi.fn(),
            onChatCommitted: vi.fn(),
        }
        const controller = createMultiuserReceiveController(dependencies)

        const character = controller.receiveCharacter(buildIncoming())
        const chat = controller.receiveChat({
            message: [{ role: 'user', data: 'after character' }],
            note: '',
            name: 'Synced',
            localLore: [],
            id: 'synced-chat',
        } as any)
        await Promise.resolve()

        expect(dependencies.upsertCompleteCharacter).toHaveBeenCalledTimes(1)
        gate.resolve()
        await Promise.all([character, chat])

        expect(events).toEqual([
            'multiuser-receive-character:start',
            'multiuser-receive-character:end',
            'activate',
            'multiuser-receive-chat:start',
            'multiuser-receive-chat:end',
        ])
        expect(temp?.chats[0].message[0].data).toBe('after character')
    })

    it('serializes chat callbacks so an older delayed commit cannot overwrite a newer chat', async () => {
        const firstGate = deferred()
        let callCount = 0
        const { dependencies, current } = createStatefulDependencies(buildCharacter())
        const controller = createMultiuserReceiveController(dependencies)
        await controller.receiveCharacter(buildIncoming())
        dependencies.upsertCompleteCharacter = vi.fn(async (_id, _reason, createOrMutate) => {
            callCount++
            const existing = current()
            const replacement = await createOrMutate(structuredClone(existing))
            if (callCount === 1) await firstGate.promise
            Object.assign(existing!, structuredClone(replacement))
            return true
        })

        const older = controller.receiveChat({
            message: [{ role: 'char', data: 'older' }],
            note: '',
            name: 'Older',
            localLore: [],
            id: 'host-chat',
        } as any)
        const newer = controller.receiveChat({
            message: [{ role: 'char', data: 'newer' }],
            note: '',
            name: 'Newer',
            localLore: [],
            id: 'host-chat',
        } as any)
        await Promise.resolve()

        expect(dependencies.upsertCompleteCharacter).toHaveBeenCalledTimes(1)
        firstGate.resolve()
        await Promise.all([older, newer])

        expect(current()?.chats[0].message[0].data).toBe('newer')
        expect(dependencies.onChatCommitted).toHaveBeenNthCalledWith(
            2,
            expect.objectContaining({ name: 'Newer' }),
        )
    })

    it('does not activate when a character commit fails', async () => {
        const commitError = new Error('commit failed')
        const dependencies: MultiuserReceiveControllerDependencies = {
            upsertCompleteCharacter: vi.fn().mockRejectedValue(commitError),
            activateCharacter: vi.fn(async () => true),
            invalidateNavigation: vi.fn(),
            deselect: vi.fn(),
            onChatCommitted: vi.fn(),
        }
        const controller = createMultiuserReceiveController(dependencies)

        await expect(controller.receiveCharacter(buildIncoming())).rejects.toBe(commitError)

        expect(dependencies.activateCharacter).not.toHaveBeenCalled()
    })

    it('does not become ready when character activation is invalidated', async () => {
        const { dependencies } = createStatefulDependencies()
        dependencies.activateCharacter = vi.fn(async () => false)
        const controller = createMultiuserReceiveController(dependencies)

        await expect(controller.receiveCharacter(buildIncoming())).rejects.toThrow(
            'Failed to activate the multiuser character',
        )
        await expect(controller.receiveChat({
            message: [{ role: 'char', data: 'must not commit' }],
            note: '',
            name: 'Rejected',
            localLore: [],
            id: 'wire-chat',
        } as any)).rejects.toThrow('not ready')

        expect(dependencies.upsertCompleteCharacter).toHaveBeenCalledOnce()
        expect(dependencies.onChatCommitted).not.toHaveBeenCalled()
    })

    it('rejects chats for this session after its character commit fails', async () => {
        let persisted = buildCharacter({ name: 'Previous session' })
        const dependencies: MultiuserReceiveControllerDependencies = {
            upsertCompleteCharacter: vi.fn()
                .mockRejectedValueOnce(new Error('character commit failed'))
                .mockImplementation(async (_id, _reason, createOrMutate) => {
                    persisted = await createOrMutate(structuredClone(persisted))
                    return true
                }),
            activateCharacter: vi.fn(async () => true),
            invalidateNavigation: vi.fn(),
            deselect: vi.fn(),
            onChatCommitted: vi.fn(),
        }
        const controller = createMultiuserReceiveController(dependencies)

        await expect(controller.receiveCharacter(buildIncoming())).rejects.toThrow(
            'character commit failed',
        )
        await expect(controller.receiveChat({
            message: [{ role: 'char', data: 'must not overwrite' }],
            note: '',
            name: 'Rejected',
            localLore: [],
            id: 'wire-chat',
        } as any)).rejects.toThrow('not ready')

        expect(dependencies.upsertCompleteCharacter).toHaveBeenCalledOnce()
        expect(persisted.name).toBe('Previous session')
        expect(persisted.chats[0].message).toEqual([])
    })

    it('continues processing later receive events after a failed commit', async () => {
        const { dependencies, current, upsert } = createStatefulDependencies()
        vi.mocked(dependencies.upsertCompleteCharacter)
            .mockRejectedValueOnce(new Error('first failed'))
            .mockImplementation(upsert)
        const controller = createMultiuserReceiveController(dependencies)

        await expect(controller.receiveCharacter(buildIncoming({ name: 'First' }))).rejects.toThrow(
            'first failed',
        )
        await expect(
            controller.receiveCharacter(buildIncoming({ name: 'Second' })),
        ).resolves.toBeUndefined()

        expect(current()?.name).toBe('Second')
        expect(dependencies.activateCharacter).toHaveBeenCalledOnce()
    })

    it('invalidates pending hydration before deselecting on close', async () => {
        const hydrationGate = deferred()
        const order: string[] = []
        let navigationGeneration = 0
        let selected: string | null = null
        const { dependencies } = createStatefulDependencies()
        dependencies.activateCharacter = vi.fn(async (id) => {
            const expectedGeneration = navigationGeneration
            await hydrationGate.promise
            if (expectedGeneration === navigationGeneration) selected = id
            return expectedGeneration === navigationGeneration
        })
        dependencies.invalidateNavigation = vi.fn(() => {
            order.push('invalidate')
            navigationGeneration++
        })
        dependencies.deselect = vi.fn(() => {
            order.push('deselect')
            selected = null
        })
        const controller = createMultiuserReceiveController(dependencies)
        const receiving = controller.receiveCharacter(buildIncoming())
        await vi.waitFor(() => expect(dependencies.activateCharacter).toHaveBeenCalledOnce())

        controller.close()
        hydrationGate.resolve()
        await receiving

        expect(order).toEqual(['invalidate', 'deselect'])
        expect(selected).toBeNull()
    })
})

describe('multiuser chat normalization', () => {
    beforeEach(() => {
        mocks.nextId = 0
    })

    it('keeps an existing chat id', () => {
        const chat = { message: [], note: '', name: 'Sync', localLore: [], id: 'kept' } as any

        expect(normalizeIncomingChat(chat, 'fallback').id).toBe('kept')
    })

    it('reuses the replaced chat id when the incoming chat has none', () => {
        const chat = { message: [], note: '', name: 'Sync', localLore: [] } as any

        expect(normalizeIncomingChat(chat, 'fallback').id).toBe('fallback')
    })

    it('generates an id when neither side has one', () => {
        const chat = { message: [], note: '', name: 'Sync', localLore: [] } as any

        expect(normalizeIncomingChat(chat, undefined).id).toBe('generated-1')
    })

    it('preserves each peer local current id across a host and joiner round trip', () => {
        const hostCurrent = {
            message: [], note: '', name: 'Host', localLore: [], id: 'host-local',
        } as any
        const joinerCurrent = {
            message: [], note: '', name: 'Joiner', localLore: [], id: 'joiner-local',
        } as any

        const atJoiner = normalizeIncomingChatForCurrent(hostCurrent, joinerCurrent)
        const backAtHost = normalizeIncomingChatForCurrent(atJoiner, hostCurrent)

        expect(atJoiner.id).toBe('joiner-local')
        expect(backAtHost.id).toBe('host-local')
        expect(hostCurrent.id).toBe('host-local')
        expect(joinerCurrent.id).toBe('joiner-local')
    })
})
