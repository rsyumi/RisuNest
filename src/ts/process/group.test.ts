import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    database: { characters: [] as any[] },
    selectedId: 0,
    selectedCharacterId: null as string | null,
    activeSession: null as import('../storage/activeConversationSession').ActiveConversationSession | null,
    doingChat: false,
    navigationGeneration: 0,
    alertConfirm: vi.fn(async () => true),
    alertSelectChar: vi.fn(async () => 'member-b'),
    activateCharacter: vi.fn(async (_id?: string) => true),
    markPersistentDataDirty: vi.fn(),
    flushPendingData: vi.fn(async () => undefined),
    reconcilePersistentActiveCharacterIds: vi.fn(),
    restoreColdPersistentCharacter: vi.fn(),
    hydrateCurrentGroupMemberDetail: vi.fn(),
    selectedTarget: null as any,
    acquireCompleteConversation: vi.fn(),
}))

vi.mock('lodash/shuffle', () => ({ default: <T>(value: T[]) => value }))
vi.mock('../util', () => ({
    findCharacterbyId: (id: string) =>
        mocks.database.characters.find((character) => character.chaId === id),
}))
vi.mock('../alert', () => ({
    alertConfirm: mocks.alertConfirm,
    alertError: vi.fn(),
    alertSelectChar: mocks.alertSelectChar,
}))
vi.mock('src/lang', () => ({ language: { askLoadFirstMsg: 'Load first message', errors: {} } }))
vi.mock('svelte/store', async (importOriginal) => {
    const original = await importOriginal<typeof import('svelte/store')>()
    return {
        ...original,
        get: (store: unknown) => store === 'doing-chat'
            ? mocks.doingChat
            : mocks.selectedId,
    }
})
vi.mock('./generationState', () => ({ doingChat: 'doing-chat' }))
vi.mock('../storage/database.svelte', () => ({
    getDatabase: () => mocks.database,
    setDatabase: vi.fn(),
}))
vi.mock('../stores.svelte', () => ({
    DBState: { db: mocks.database },
    selectedCharID: {},
}))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    captureSelectedConversationTarget: () => mocks.selectedTarget,
    acquireCompleteConversation: mocks.acquireCompleteConversation,
    activateCharacter: mocks.activateCharacter,
    flushPendingData: mocks.flushPendingData,
    getPersistentNavigationGeneration: () => mocks.navigationGeneration,
    getActiveConversationSession: () => mocks.activeSession,
    markPersistentDataDirty: mocks.markPersistentDataDirty,
    reconcilePersistentActiveCharacterIds: mocks.reconcilePersistentActiveCharacterIds,
    hydrateCurrentGroupMemberDetail: mocks.hydrateCurrentGroupMemberDetail,
}))
vi.mock('./coldCharacterRestore', () => ({
    restoreColdPersistentCharacter: mocks.restoreColdPersistentCharacter,
}))

import { addGroupChar, groupOrder, rmCharFromGroup } from './group'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import { createCatalogCharacterStub } from '../storage/workingSetCatalog'

function makeGroup() {
    return {
        type: 'group',
        chaId: 'group-a',
        characters: ['member-a'],
        characterTalks: [0.5],
        characterActive: [true],
        chats: [{ message: [], id: 'chat-a' }],
        chatPage: 0,
    }
}

function deferred<T>() {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((resolvePromise) => {
        resolve = resolvePromise
    })
    return { promise, resolve }
}

describe('group working-set residency', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        mocks.navigationGeneration = 0
        mocks.selectedId = 0
        mocks.selectedCharacterId = null
        mocks.activeSession = null
        mocks.doingChat = false
        const group = makeGroup()
        mocks.database.characters = [
            group,
            { type: 'character', chaId: 'member-a', name: 'Alpha', firstMessage: 'A', chats: [] },
            { type: 'character', chaId: 'member-b', name: 'Beta', chats: [] },
        ]
        mocks.alertConfirm.mockResolvedValue(true)
        mocks.alertSelectChar.mockResolvedValue('member-b')
        mocks.restoreColdPersistentCharacter.mockResolvedValue({
            type: 'character',
            chaId: 'member-b',
            firstMessage: 'Hydrated B',
        })
        mocks.hydrateCurrentGroupMemberDetail.mockImplementation((groupId, detail) => {
            const group = mocks.database.characters[mocks.selectedId]
            if (group?.chaId !== groupId || group.type !== 'group') return false
            const index = mocks.database.characters.findIndex(
                (character) => character.chaId === detail.chaId,
            )
            if (index < 0) return false
            const chats = mocks.database.characters[index].chats
            mocks.database.characters[index] = { ...detail, chats }
            return true
        })
        mocks.activateCharacter.mockImplementation(async (id) => {
            mocks.navigationGeneration++
            mocks.selectedCharacterId = id
            return true
        })
        mocks.selectedTarget = null
        mocks.acquireCompleteConversation.mockReset()
        mocks.flushPendingData.mockReset()
        mocks.flushPendingData.mockResolvedValue(undefined)
    })

    it('hydrates a newly added member before using its first message', async () => {
        await addGroupChar()

        const group = mocks.database.characters[0]
        expect(mocks.activateCharacter).toHaveBeenCalledWith('group-a')
        expect(group.characters).toEqual(['member-a', 'member-b'])
        expect(group.characterTalks).toEqual([0.5, 1 / 6 * 4])
        expect(group.characterActive).toEqual([true, true])
        expect(group.chats[0].message).toEqual([
            expect.objectContaining({
                role: 'char',
                data: 'Hydrated B',
                saying: 'member-b',
                chatId: expect.any(String),
            }),
        ])
        expect(mocks.markPersistentDataDirty).toHaveBeenCalled()
    })

    it('publishes restored scalable member detail before immediate group generation', async () => {
        const detail = {
            type: 'character',
            chaId: 'member-b',
            name: 'Beta',
            personality: 'Persistent personality',
            scenario: 'Persistent scenario',
            firstMessage: 'Persistent greeting',
        }
        mocks.database.characters[2] = createCatalogCharacterStub({
            id: 'member-b',
            configuredIndex: 2,
            conversationCount: 0,
            name: 'Beta',
            type: 'character',
            recentAt: 0,
            trashed: false,
        })
        mocks.restoreColdPersistentCharacter.mockResolvedValue(detail)
        mocks.alertConfirm.mockResolvedValue(false)

        await expect(addGroupChar()).resolves.toBe(true)

        expect(mocks.hydrateCurrentGroupMemberDetail).toHaveBeenCalledWith(
            'group-a',
            detail,
        )
        const order = groupOrder([
            { id: 'member-b', talkness: 1, index: 0 },
        ], 'persistent')
        const generatedMember = mocks.database.characters.find(
            (character) => character.chaId === order[0].id,
        )
        expect(generatedMember).toMatchObject({
            personality: 'Persistent personality',
            scenario: 'Persistent scenario',
        })
    })

    it('publishes the current member revision when activation races a newer detail', async () => {
        let persistentDetail = {
            type: 'character',
            chaId: 'member-b',
            name: 'Beta',
            personality: 'Revision R personality',
            scenario: 'Revision R prompt',
        }
        mocks.database.characters[2] = createCatalogCharacterStub({
            id: 'member-b',
            configuredIndex: 2,
            conversationCount: 0,
            name: 'Beta',
            type: 'character',
            recentAt: 0,
            trashed: false,
        })
        mocks.restoreColdPersistentCharacter.mockImplementation(async () => ({
            ...persistentDetail,
        }))
        mocks.alertConfirm.mockResolvedValue(false)
        let activationAttempt = 0
        mocks.activateCharacter.mockImplementation(async () => {
            activationAttempt++
            if (activationAttempt === 1) {
                persistentDetail = {
                    ...persistentDetail,
                    personality: 'Revision R+1 personality',
                    scenario: 'Revision R+1 prompt',
                }
            }
            mocks.navigationGeneration++
            return activationAttempt === 2
        })

        await expect(addGroupChar()).resolves.toBe(true)

        expect(mocks.database.characters[2]).toMatchObject({
            personality: 'Revision R+1 personality',
            scenario: 'Revision R+1 prompt',
        })
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
        expect(mocks.restoreColdPersistentCharacter.mock.invocationCallOrder[0]).toBeGreaterThan(
            mocks.activateCharacter.mock.invocationCallOrder[1],
        )
    })

    it('routes a first-message greeting through the active conversation session', async () => {
        const group = mocks.database.characters[0]
        const onMutation = vi.fn()
        mocks.activeSession = new ActiveConversationSession({
            characterId: group.chaId,
            conversationId: group.chats[0].id,
            conversation: group.chats[0],
            storeRevision: 1,
            onMutation,
        })

        await expect(addGroupChar()).resolves.toBe(true)

        expect(onMutation).toHaveBeenCalledWith(expect.objectContaining({
            commands: ['append'],
        }))
    })

    it('activates before promotion and holds one exact lease through synchronous greeting commit', async () => {
        const group = mocks.database.characters[0]
        const session = new ActiveConversationSession({
            characterId: group.chaId,
            conversationId: group.chats[0].id,
            conversation: group.chats[0],
            storeRevision: 1,
        })
        mocks.activeSession = session
        const target = { characterId: group.chaId, conversationId: group.chats[0].id }
        mocks.selectedTarget = target
        let resolvePromotion!: (lease: any) => void
        mocks.acquireCompleteConversation.mockReturnValue(new Promise((resolve) => {
            resolvePromotion = resolve
        }))
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            return true
        })
        let releaseCount = 0

        const adding = addGroupChar()
        for (let index = 0; index < 20; index++) await Promise.resolve()

        expect(mocks.acquireCompleteConversation).toHaveBeenCalledOnce()
        expect(group.characters).toEqual(['member-a'])
        expect(group.chats[0].message).toEqual([])

        const pin = session.acquirePin('compatibility')
        resolvePromotion({
            session,
            target,
            release() {
                releaseCount += 1
                pin.release()
            },
        })
        await expect(adding).resolves.toBe(true)
        expect(group.characters).toEqual(['member-a', 'member-b'])
        expect(group.chats[0].message).toEqual([
            expect.objectContaining({ data: 'Hydrated B', chatId: expect.any(String) }),
        ])
        expect(releaseCount).toBe(1)
        expect(session.pinCount('compatibility')).toBe(0)
    })

    it('rechecks duplicate membership after awaited promotion before mutating', async () => {
        const group = mocks.database.characters[0]
        const session = new ActiveConversationSession({
            characterId: group.chaId,
            conversationId: group.chats[0].id,
            conversation: group.chats[0],
            storeRevision: 1,
        })
        mocks.activeSession = session
        const target = { characterId: group.chaId, conversationId: group.chats[0].id }
        mocks.selectedTarget = target
        const promotion = deferred<any>()
        mocks.acquireCompleteConversation.mockReturnValue(promotion.promise)
        let releaseCount = 0

        const adding = addGroupChar()
        while (mocks.acquireCompleteConversation.mock.calls.length === 0) await Promise.resolve()
        group.characters.push('member-b')
        group.characterTalks.push(0.75)
        group.characterActive.push(false)
        promotion.resolve({
            session,
            target,
            release() { releaseCount += 1 },
        })

        await expect(adding).resolves.toBe(false)
        expect(group.characters).toEqual(['member-a', 'member-b'])
        expect(group.characterTalks).toEqual([0.5, 0.75])
        expect(group.characterActive).toEqual([true, false])
        expect(group.chats[0].message).toEqual([])
        expect(mocks.activateCharacter).toHaveBeenCalledOnce()
        expect(releaseCount).toBe(1)
    })

    it('does not mutate membership or greeting when activation is superseded', async () => {
        const group = mocks.database.characters[0]
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration += 2
            return false
        })

        await expect(addGroupChar()).resolves.toBe(false)

        expect(group.characters).toEqual(['member-a'])
        expect(group.characterTalks).toEqual([0.5])
        expect(group.characterActive).toEqual([true])
        expect(group.chats[0].message).toEqual([])
        expect(mocks.markPersistentDataDirty).not.toHaveBeenCalled()
        expect(mocks.flushPendingData).not.toHaveBeenCalled()
    })

    it('activates first and commits membership plus greeting to the replacement current owner', async () => {
        const oldGroup = mocks.database.characters[0]
        let replacement: ReturnType<typeof makeGroup> | null = null
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            replacement = makeGroup()
            mocks.database.characters[0] = replacement
            mocks.activeSession = new ActiveConversationSession({
                characterId: replacement.chaId,
                conversationId: replacement.chats[0].id,
                conversation: replacement.chats[0] as any,
                storeRevision: 1,
            })
            return true
        })

        await expect(addGroupChar()).resolves.toBe(true)

        expect(oldGroup.characters).toEqual(['member-a'])
        expect(oldGroup.chats[0].message).toEqual([])
        expect(replacement!.characters).toEqual(['member-a', 'member-b'])
        expect(replacement!.chats[0].message).toEqual([
            expect.objectContaining({
                role: 'char',
                data: 'Hydrated B',
                saying: 'member-b',
                chatId: expect.any(String),
            }),
        ])
        expect(mocks.markPersistentDataDirty).toHaveBeenCalledOnce()
    })

    it('leaves both the old owner and a superseding catalog object clean before returning', async () => {
        const oldGroup = mocks.database.characters[0]
        const catalogReplacement = {
            ...makeGroup(),
            name: 'Concurrent catalog replacement',
        }
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration += 2
            mocks.database.characters[0] = catalogReplacement
            return false
        })

        await expect(addGroupChar()).resolves.toBe(false)

        expect(oldGroup.characters).toEqual(['member-a'])
        expect(oldGroup.chats[0].message).toEqual([])
        expect(catalogReplacement.characters).toEqual(['member-a'])
        expect(catalogReplacement.chats[0].message).toEqual([])
        expect(mocks.markPersistentDataDirty).not.toHaveBeenCalled()
        expect(mocks.flushPendingData).not.toHaveBeenCalled()
    })

    it('does not touch an existing identical greeting when activation fails', async () => {
        const group = mocks.database.characters[0]
        const identical = { role: 'char', data: 'Hydrated B', saying: 'member-b' }
        group.chats[0].message.push(identical)
        const session = new ActiveConversationSession({
            characterId: group.chaId,
            conversationId: group.chats[0].id,
            conversation: group.chats[0],
            storeRevision: 1,
        })
        mocks.activeSession = session
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            return false
        })

        await expect(addGroupChar()).resolves.toBe(false)

        expect(group.characters).toEqual(['member-a'])
        expect(group.chats[0].message).toEqual([identical])
        expect(session.version).toBe(0)
    })

    it('releases once when post-promotion owner validation throws', async () => {
        const group = mocks.database.characters[0]
        const session = new ActiveConversationSession({
            characterId: group.chaId,
            conversationId: group.chats[0].id,
            conversation: group.chats[0],
            storeRevision: 1,
        })
        const target = { characterId: group.chaId, conversationId: group.chats[0].id }
        mocks.selectedTarget = target
        const promotion = deferred<any>()
        mocks.acquireCompleteConversation.mockReturnValue(promotion.promise)
        const failure = new Error('chat validation failed')
        let releaseCount = 0

        const adding = addGroupChar()
        while (mocks.acquireCompleteConversation.mock.calls.length === 0) await Promise.resolve()
        Object.defineProperty(group, 'chats', {
            configurable: true,
            get() { throw failure },
        })
        promotion.resolve({
            session,
            target,
            release() { releaseCount += 1 },
        })

        await expect(adding).rejects.toBe(failure)
        expect(releaseCount).toBe(1)
    })

    it('aborts group membership and greeting mutation when the promoted lease is stale', async () => {
        const group = mocks.database.characters[0]
        const otherChat = { id: 'chat-b', message: [] }
        const otherSession = new ActiveConversationSession({
            characterId: group.chaId,
            conversationId: otherChat.id,
            conversation: otherChat as any,
            storeRevision: 1,
        })
        mocks.selectedTarget = { characterId: group.chaId, conversationId: group.chats[0].id }
        let releaseCount = 0
        mocks.acquireCompleteConversation.mockResolvedValue({
            session: otherSession,
            target: mocks.selectedTarget,
            release() { releaseCount += 1 },
        })

        await expect(addGroupChar()).resolves.toBe(false)

        expect(group.characters).toEqual(['member-a'])
        expect(group.chats[0].message).toEqual([])
        expect(mocks.activateCharacter).toHaveBeenCalledOnce()
        expect(releaseCount).toBe(1)
    })

    it('does not append a first-message greeting when activation fails', async () => {
        const group = mocks.database.characters[0]
        const onMutation = vi.fn()
        mocks.activeSession = new ActiveConversationSession({
            characterId: group.chaId,
            conversationId: group.chats[0].id,
            conversation: group.chats[0],
            storeRevision: 1,
            onMutation,
        })
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            return false
        })

        await expect(addGroupChar()).resolves.toBe(false)

        expect(group.chats[0].message).toEqual([])
        expect(onMutation).not.toHaveBeenCalled()
    })

    it('orders group generation from detail-only members without conversation histories', () => {
        const order = groupOrder([
            { id: 'member-a', talkness: 1, index: 0 },
            { id: 'member-b', talkness: 1, index: 1 },
        ], 'alpha')

        expect(order[0].id).toBe('member-a')
        expect(mocks.database.characters[1].chats).toEqual([])
        expect(mocks.database.characters[2].chats).toEqual([])
    })

    it('does not mutate membership when generation starts during cold restore', async () => {
        mocks.restoreColdPersistentCharacter.mockImplementationOnce(async () => {
            mocks.doingChat = true
            return {
                type: 'character',
                chaId: 'member-b',
                firstMessage: 'Hydrated B',
            }
        })

        await expect(addGroupChar()).resolves.toBe(false)

        expect(mocks.database.characters[0].characters).toEqual(['member-a'])
        expect(mocks.database.characters[0].chats[0].message).toEqual([])
        expect(mocks.activateCharacter).toHaveBeenCalledOnce()
        expect(mocks.markPersistentDataDirty).not.toHaveBeenCalled()
    })

    it('keeps membership and the requested first message atomic when activation is stale', async () => {
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            return false
        })

        await expect(addGroupChar()).resolves.toBe(false)

        const group = mocks.database.characters[0]
        expect(mocks.restoreColdPersistentCharacter).not.toHaveBeenCalled()
        expect(group.characters).toEqual(['member-a'])
        expect(group.chats[0].message).toEqual([])
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
        expect(mocks.flushPendingData).not.toHaveBeenCalled()
        expect(mocks.reconcilePersistentActiveCharacterIds).not.toHaveBeenCalled()
    })

    it('rolls back membership when busy generation blocks activation before navigation claim', async () => {
        mocks.activateCharacter.mockResolvedValue(false)

        await expect(addGroupChar()).resolves.toBe(false)

        const group = mocks.database.characters[0]
        expect(group.characters).toEqual(['member-a'])
        expect(group.chats[0].message).toEqual([])
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
        expect(mocks.flushPendingData).not.toHaveBeenCalled()
        expect(mocks.reconcilePersistentActiveCharacterIds).not.toHaveBeenCalled()
    })

    it('rolls back membership when same-group activation rejects', async () => {
        mocks.activateCharacter.mockRejectedValue(new Error('member hydration failed'))

        await expect(addGroupChar()).resolves.toBe(false)

        const group = mocks.database.characters[0]
        expect(group.characters).toEqual(['member-a'])
        expect(group.chats[0].message).toEqual([])
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
        expect(mocks.flushPendingData).not.toHaveBeenCalled()
    })

    it('removes only the newly appended first message when rollback sees duplicate content', async () => {
        const group = mocks.database.characters[0]
        const existingMessage = {
            role: 'char',
            data: 'Hydrated B',
            saying: 'member-b',
        }
        group.chats[0].message.push(existingMessage)
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            return false
        })

        await expect(addGroupChar()).resolves.toBe(false)

        expect(group.chats[0].message).toEqual([existingMessage])
    })

    it('retries same-group activation before accepting an added member', async () => {
        const results = [false, true]
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            return results.shift() ?? false
        })

        await expect(addGroupChar()).resolves.toBe(true)

        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
        expect(mocks.database.characters[0].characters).toEqual(['member-a', 'member-b'])
        expect(mocks.flushPendingData).not.toHaveBeenCalled()
    })

    it('reactivates a group after removing a member so residency is reconciled', async () => {
        const group = mocks.database.characters[0]
        group.characters.push('member-b')
        group.characterTalks.push(0.75)
        group.characterActive.push(false)
        mocks.activateCharacter.mockResolvedValue(true)

        await expect(rmCharFromGroup(0)).resolves.toBe(true)

        expect(group.characters).toEqual(['member-b'])
        expect(group.characterTalks).toEqual([0.75])
        expect(group.characterActive).toEqual([false])
        expect(mocks.markPersistentDataDirty).toHaveBeenCalledOnce()
        expect(mocks.activateCharacter).toHaveBeenCalledWith('group-a')
    })

    it('does not remove a member while generation is busy', async () => {
        const group = mocks.database.characters[0]
        group.characters.push('member-b')
        group.characterTalks.push(0.75)
        group.characterActive.push(false)
        mocks.doingChat = true

        await expect(rmCharFromGroup(0)).resolves.toBe(false)

        expect(group.characters).toEqual(['member-a', 'member-b'])
        expect(mocks.activateCharacter).not.toHaveBeenCalled()
        expect(mocks.markPersistentDataDirty).not.toHaveBeenCalled()
    })

    it('reconciles active residency for the completed removal when activation is superseded', async () => {
        const group = mocks.database.characters[0]
        group.characters.push('member-b')
        group.characterTalks.push(0.75)
        group.characterActive.push(false)
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.selectedId = 1
            return false
        })

        await expect(rmCharFromGroup(0)).resolves.toBe(false)

        expect(group.characters).toEqual(['member-b'])
        expect(group.characterTalks).toEqual([0.75])
        expect(group.characterActive).toEqual([false])
        expect(mocks.reconcilePersistentActiveCharacterIds).toHaveBeenCalledWith(
            mocks.database,
            'member-a',
        )
        expect(mocks.flushPendingData).not.toHaveBeenCalled()
    })

    it('rolls back a removed member when same-group activation stays stale', async () => {
        const group = mocks.database.characters[0]
        group.characters.push('member-b')
        group.characterTalks.push(0.75)
        group.characterActive.push(false)
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            return false
        })

        await expect(rmCharFromGroup(0)).resolves.toBe(false)

        expect(group.characters).toEqual(['member-a', 'member-b'])
        expect(group.characterTalks).toEqual([0.5, 0.75])
        expect(group.characterActive).toEqual([true, false])
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
        expect(mocks.flushPendingData).toHaveBeenCalledWith('group-membership-rollback')
        expect(mocks.reconcilePersistentActiveCharacterIds).toHaveBeenCalled()
    })

    it('rolls back a removed member when busy generation blocks navigation claim', async () => {
        const group = mocks.database.characters[0]
        group.characters.push('member-b')
        group.characterTalks.push(0.75)
        group.characterActive.push(false)
        mocks.activateCharacter.mockResolvedValue(false)

        await expect(rmCharFromGroup(0)).resolves.toBe(false)

        expect(group.characters).toEqual(['member-a', 'member-b'])
        expect(group.characterTalks).toEqual([0.5, 0.75])
        expect(group.characterActive).toEqual([true, false])
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
        expect(mocks.flushPendingData).toHaveBeenCalledWith('group-membership-rollback')
    })

    it('rolls back a removed member when same-group activation rejects', async () => {
        const group = mocks.database.characters[0]
        group.characters.push('member-b')
        group.characterTalks.push(0.75)
        group.characterActive.push(false)
        mocks.activateCharacter.mockRejectedValue(new Error('member hydration failed'))

        await expect(rmCharFromGroup(0)).resolves.toBe(false)

        expect(group.characters).toEqual(['member-a', 'member-b'])
        expect(group.characterTalks).toEqual([0.5, 0.75])
        expect(group.characterActive).toEqual([true, false])
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
        expect(mocks.flushPendingData).toHaveBeenCalledWith('group-membership-rollback')
    })

    it('does not duplicate a removed member restored by a concurrent group replacement', async () => {
        const group = mocks.database.characters[0]
        group.characters.push('member-b')
        group.characterTalks.push(0.75)
        group.characterActive.push(false)
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            mocks.database.characters[0] = {
                ...makeGroup(),
                characters: ['member-a', 'member-b'],
                characterTalks: [0.5, 0.75],
                characterActive: [true, false],
            }
            return false
        })

        await expect(rmCharFromGroup(0)).resolves.toBe(false)

        expect(mocks.database.characters[0].characters).toEqual(['member-a', 'member-b'])
        expect(mocks.database.characters[0].characterTalks).toEqual([0.5, 0.75])
        expect(mocks.database.characters[0].characterActive).toEqual([true, false])
    })
})
