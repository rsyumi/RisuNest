import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    readPersistentCompleteCharacter: vi.fn(),
}))

vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    readPersistentCompleteCharacter: mocks.readPersistentCompleteCharacter,
}))
vi.mock('../storage/database.svelte', () => ({
    getCurrentCharacter: () => ({
        firstMessage: 'Resident greeting',
        alternateGreetings: [],
        chats: [],
    }),
}))

import { getChatBranches } from './branches'

describe('chat branch visualization', () => {
    beforeEach(() => {
        vi.clearAllMocks()
    })

    it('includes nonselected conversation branches from the authoritative character', async () => {
        mocks.readPersistentCompleteCharacter.mockResolvedValue({
            type: 'character',
            chaId: 'char-a',
            name: 'Character',
            firstMessage: 'Greeting',
            alternateGreetings: [],
            chats: [
                {
                    id: 'chat-a',
                    name: 'Selected',
                    note: '',
                    localLore: [],
                    fmIndex: -1,
                    message: [{ role: 'user', data: 'selected path' }],
                },
                {
                    id: 'chat-b',
                    name: 'Nonselected',
                    note: '',
                    localLore: [],
                    fmIndex: -1,
                    message: [{ role: 'user', data: 'nonselected path' }],
                },
            ],
            chatPage: 0,
        })

        const branches = await getChatBranches('char-a')

        expect(mocks.readPersistentCompleteCharacter).toHaveBeenCalledWith(
            'char-a',
            'chat-branches',
        )
        expect(branches.map((branch) => branch.preview)).toContain('nonselected path')
    })
})
