import { expect, test, vi } from 'vitest'
import {
    defaultCBSRegisterArg,
    registerCBS,
    type matcherArg,
    type RegisterCallback,
} from './cbs'
import type { Database } from './storage/database.svelte'

vi.mock('./stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return { CurrentTriggerIdStore: writable(null) }
})

const database = (lastCharacterMessage: string) => ({
    characters: [{
        chatPage: 0,
        chats: [{
            fmIndex: -1,
            message: [
                { role: 'user', data: 'user' },
                { role: 'char', data: lastCharacterMessage },
            ],
        }],
        firstMessage: 'first',
    }],
}) as Database

test('history CBS callbacks read the operation database carried by matcherArg', () => {
    const callbacks = new Map<string, RegisterCallback>()
    registerCBS({
        ...defaultCBSRegisterArg,
        getDatabase: () => database('live-concurrent-value'),
        getSelectedCharID: () => 0,
        registerFunction: ({ name, callback, alias }) => {
            if (callback === 'doc_only') return
            for (const key of [name, ...alias]) callbacks.set(key, callback)
        },
    })
    const callback = callbacks.get('previouscharchat')!
    const operationDatabase = database('pinned-operation-value')

    const result = callback('', {
        chatID: -1,
        db: operationDatabase,
        chara: operationDatabase.characters[0],
        rmVar: false,
        cbsConditions: {},
    } as matcherArg, [], null)

    expect(result).toBe('pinned-operation-value')
})
