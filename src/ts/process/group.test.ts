import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    database: { characters: [] as any[] },
    selectedId: 0,
    selectedCharacterId: null as string | null,
    doingChat: false,
    navigationGeneration: 0,
    alertConfirm: vi.fn(async () => true),
    alertSelectChar: vi.fn(async () => 'member-b'),
    activateCharacter: vi.fn(async (_id?: string) => true),
    markPersistentDataDirty: vi.fn(),
    flushPendingData: vi.fn(async () => undefined),
    reconcilePersistentActiveCharacterIds: vi.fn(),
    restoreColdPersistentCharacter: vi.fn(),
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
    activateCharacter: mocks.activateCharacter,
    flushPendingData: mocks.flushPendingData,
    getPersistentNavigationGeneration: () => mocks.navigationGeneration,
    markPersistentDataDirty: mocks.markPersistentDataDirty,
    reconcilePersistentActiveCharacterIds: mocks.reconcilePersistentActiveCharacterIds,
}))
vi.mock('./coldCharacterRestore', () => ({
    restoreColdPersistentCharacter: mocks.restoreColdPersistentCharacter,
}))

import { addGroupChar, rmCharFromGroup } from './group'

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

describe('group working-set residency', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        mocks.navigationGeneration = 0
        mocks.selectedId = 0
        mocks.selectedCharacterId = null
        mocks.doingChat = false
        const group = makeGroup()
        mocks.database.characters = [
            group,
            { type: 'character', chaId: 'member-a', firstMessage: 'A', chats: [] },
            { type: 'character', chaId: 'member-b', chats: [] },
        ]
        mocks.alertConfirm.mockResolvedValue(true)
        mocks.alertSelectChar.mockResolvedValue('member-b')
        mocks.restoreColdPersistentCharacter.mockResolvedValue({
            type: 'character',
            chaId: 'member-b',
            firstMessage: 'Hydrated B',
        })
        mocks.activateCharacter.mockImplementation(async (id) => {
            mocks.navigationGeneration++
            mocks.selectedCharacterId = id
            return true
        })
    })

    it('hydrates a newly added member before using its first message', async () => {
        await addGroupChar()

        const group = mocks.database.characters[0]
        expect(mocks.activateCharacter).toHaveBeenCalledWith('group-a')
        expect(group.characters).toEqual(['member-a', 'member-b'])
        expect(group.characterTalks).toEqual([0.5, 1 / 6 * 4])
        expect(group.characterActive).toEqual([true, true])
        expect(group.chats[0].message).toEqual([
            { role: 'char', data: 'Hydrated B', saying: 'member-b' },
        ])
        expect(mocks.markPersistentDataDirty).toHaveBeenCalled()
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
        expect(mocks.activateCharacter).not.toHaveBeenCalled()
        expect(mocks.markPersistentDataDirty).not.toHaveBeenCalled()
    })

    it('keeps membership and the requested first message atomic when activation is stale', async () => {
        mocks.activateCharacter.mockImplementation(async () => {
            mocks.navigationGeneration++
            return false
        })

        await expect(addGroupChar()).resolves.toBe(false)

        const group = mocks.database.characters[0]
        expect(mocks.restoreColdPersistentCharacter).toHaveBeenCalledWith(
            'member-b',
            expect.objectContaining({ errorMessage: undefined }),
        )
        expect(group.characters).toEqual(['member-a'])
        expect(group.chats[0].message).toEqual([])
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
        expect(mocks.flushPendingData).toHaveBeenCalledWith('group-membership-rollback')
        expect(mocks.reconcilePersistentActiveCharacterIds).toHaveBeenCalled()
    })

    it('rolls back membership when busy generation blocks activation before navigation claim', async () => {
        mocks.activateCharacter.mockResolvedValue(false)

        await expect(addGroupChar()).resolves.toBe(false)

        const group = mocks.database.characters[0]
        expect(group.characters).toEqual(['member-a'])
        expect(group.chats[0].message).toEqual([])
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
        expect(mocks.flushPendingData).toHaveBeenCalledWith('group-membership-rollback')
        expect(mocks.reconcilePersistentActiveCharacterIds).toHaveBeenCalled()
    })

    it('rolls back membership when same-group activation rejects', async () => {
        mocks.activateCharacter.mockRejectedValue(new Error('member hydration failed'))

        await expect(addGroupChar()).resolves.toBe(false)

        const group = mocks.database.characters[0]
        expect(group.characters).toEqual(['member-a'])
        expect(group.chats[0].message).toEqual([])
        expect(mocks.activateCharacter).toHaveBeenCalledTimes(2)
        expect(mocks.flushPendingData).toHaveBeenCalledWith('group-membership-rollback')
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
