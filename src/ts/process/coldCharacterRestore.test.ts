import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    readPersistentCharacterDetail: vi.fn(),
    replacePersistentCompleteCharacter: vi.fn(),
    getColdStorageItem: vi.fn(),
}))

vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    readPersistentCharacterDetail: mocks.readPersistentCharacterDetail,
    replacePersistentCompleteCharacter: mocks.replacePersistentCompleteCharacter,
}))
vi.mock('./coldstorage.svelte', () => ({
    getColdStorageItem: mocks.getColdStorageItem,
}))

import { restoreColdPersistentCharacter } from './coldCharacterRestore'

describe('cold persistent character restore', () => {
    beforeEach(() => {
        vi.clearAllMocks()
    })

    it('restores a complete cold member before returning its definition and first message', async () => {
        const restored = {
            type: 'character',
            chaId: 'member-cold',
            name: 'Cold member',
            firstMessage: 'Complete greeting',
            desc: 'Complete definition',
            chats: [{ id: 'chat-a', message: [] }],
        }
        mocks.readPersistentCharacterDetail.mockResolvedValue({
            type: 'character',
            chaId: 'member-cold',
            name: 'Cold shell',
            coldstorage: 'cold-key',
        })
        mocks.getColdStorageItem.mockResolvedValue({ character: restored })
        mocks.replacePersistentCompleteCharacter.mockImplementation(
            async (_id, _reason, mutate) => {
                expect(await mutate({
                    type: 'character',
                    chaId: 'member-cold',
                    name: 'Cold shell',
                    coldstorage: 'cold-key',
                    chats: [],
                })).toBe(restored)
                return true
            },
        )

        await expect(restoreColdPersistentCharacter('member-cold', {
            errorMessage: 'restore failed',
            isCurrent: () => true,
        })).resolves.toEqual(restored)

        expect(mocks.replacePersistentCompleteCharacter).toHaveBeenCalledWith(
            'member-cold',
            'cold-character-restore',
            expect.any(Function),
        )
    })

    it('keeps a concurrently hydrated character instead of overwriting it with stale cold data', async () => {
        const shell = {
            type: 'character',
            chaId: 'member-cold',
            name: 'Cold shell',
            coldstorage: 'cold-key',
        }
        const staleColdCharacter = {
            type: 'character',
            chaId: 'member-cold',
            name: 'Stale cold body',
            chats: [],
        }
        const concurrentCharacter = {
            type: 'character',
            chaId: 'member-cold',
            name: 'Edited after hydration',
            chats: [{ id: 'chat-new', message: [] }],
        }
        mocks.readPersistentCharacterDetail.mockResolvedValue(shell)
        mocks.getColdStorageItem.mockResolvedValue({ character: staleColdCharacter })
        mocks.replacePersistentCompleteCharacter.mockImplementation(
            async (_id, _reason, mutate) => {
                await mutate(concurrentCharacter)
                return true
            },
        )

        await expect(restoreColdPersistentCharacter('member-cold', {
            errorMessage: 'restore failed',
            isCurrent: () => true,
        })).resolves.toBe(concurrentCharacter)
    })
})
