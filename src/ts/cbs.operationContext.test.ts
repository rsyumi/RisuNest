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

test('history CBS callbacks resolve the captured character instead of global selection', () => {
    const callbacks = new Map<string, RegisterCallback>()
    const operationDatabase = {
        characters: [
            {
                chaId: 'character-a',
                chatPage: 0,
                chats: [{
                    fmIndex: -1,
                    message: [{ role: 'char', data: 'captured-character-value' }],
                }],
                firstMessage: 'captured-first',
            },
            {
                chaId: 'character-b',
                chatPage: 0,
                chats: [{
                    fmIndex: -1,
                    message: [{ role: 'char', data: 'global-selection-value' }],
                }],
                firstMessage: 'global-first',
            },
        ],
    } as Database
    registerCBS({
        ...defaultCBSRegisterArg,
        getDatabase: () => operationDatabase,
        getSelectedCharID: () => 1,
        registerFunction: ({ name, callback, alias }) => {
            if (callback === 'doc_only') return
            for (const key of [name, ...alias]) callbacks.set(key, callback)
        },
    })

    const result = callbacks.get('previouscharchat')!('', {
        chatID: -1,
        db: operationDatabase,
        chara: operationDatabase.characters[0],
        selectedCharacterId: 'character-a',
        rmVar: false,
        cbsConditions: {},
    } as matcherArg, [], null)

    expect(result).toBe('captured-character-value')
})
