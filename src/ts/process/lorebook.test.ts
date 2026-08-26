import { beforeEach, describe, expect, test, vi } from 'vitest'

vi.mock('../parser/chatVar.svelte', () => ({
    getChatVar: vi.fn(),
    setChatVar: vi.fn(),
}))

vi.mock('../stores.svelte', () => ({
    DBState: { db: {} },
    selectedCharID: {
        subscribe: (run: (value: number) => void) => {
            run(0)
            return () => undefined
        },
    },
}))

vi.mock('../tokenizer', () => ({
    tokenize: vi.fn(async (text: string) => text.length),
}))

vi.mock('../parser/parser.svelte', () => ({
    risuChatParser: vi.fn((text: string) => text),
}))

vi.mock('../util', () => ({
    findCharacterbyId: vi.fn(),
    pickHashRand: vi.fn(() => 0),
    selectSingleFile: vi.fn(),
}))

vi.mock('../alert', () => ({ alertError: vi.fn(), alertNormal: vi.fn() }))
vi.mock('../../lang', () => ({ language: { successExport: 'exported' } }))
vi.mock('../globalApi.svelte', () => ({ downloadFile: vi.fn(), saveAsset: vi.fn() }))
vi.mock('./modules', () => ({ getModuleLorebooks: vi.fn(() => []) }))

import { DBState } from '../stores.svelte'
import type { Chat } from '../storage/database.svelte'
import { ActiveConversationSession } from '../storage/activeConversationSession'
import {
    beginPinnedConversationHistoryOperation,
    createCompatibilityConversationHistorySnapshot,
} from '../storage/conversationHistoryOperation'
import { loadLoreBookV3Prompt } from './lorebook.svelte'

type Lore = {
    key: string
    content: string
    comment?: string
    alwaysActive?: boolean
    secondkey?: string
    selective?: boolean
    useRegex?: boolean
    insertorder?: number
    mode?: 'normal' | 'child'
    id?: string
}

function setLorebook(lore: Lore[], messages = [{ role: 'user', data: '' }], tokenBudget = 10_000) {
    DBState.db = {
        username: 'User',
        loreBookDepth: 10,
        loreBookToken: tokenBudget,
        characters: [{
            name: 'Char',
            chatPage: 0,
            globalLore: lore.map((entry) => ({
                comment: '',
                alwaysActive: false,
                secondkey: '',
                selective: false,
                insertorder: 100,
                mode: 'normal',
                ...entry,
            })),
            chats: [{ localLore: [], message: messages }],
            loreSettings: { recursiveScanning: true },
        }],
    } as never
}

async function activePrompts() {
    const character = DBState.db.characters[0]
    const operation = createCompatibilityConversationHistorySnapshot({
        characterId: character.chaId ?? 'character-test',
        conversationId: character.chats[0].id ?? 'conversation-test',
        messages: character.chats[0].message,
        storeRevision: 0,
    })
    try {
        const result = await loadLoreBookV3Prompt(operation)
        return result.actives.map((active) => active.prompt)
    } finally {
        operation.dispose()
    }
}

beforeEach(() => {
    vi.restoreAllMocks()
})

describe('loadLoreBookV3Prompt characterization', () => {
    test('normalizes case, comments, and spaces for partial matching', async () => {
        setLorebook(
            [{ key: 'alpha beta', content: 'matched' }],
            [{ role: 'user', data: 'ALPHA{{// hidden}} beta' }],
        )

        await expect(activePrompts()).resolves.toEqual(['matched'])
    })

    test('uses split(\' \') semantics for full-word matching', async () => {
        setLorebook(
            [
                { key: 'alpha', content: 'space-match' },
                { key: 'beta', content: '@@match_full_word\ntab-no-match' },
            ],
            [{ role: 'user', data: 'alpha  beta\tgamma' }],
        )

        await expect(activePrompts()).resolves.toEqual(['space-match'])
    })

    test('requires selective keys and rejects entries with matching negative keys', async () => {
        setLorebook(
            [
                { key: 'alpha', secondkey: 'beta', selective: true, content: 'selective' },
                { key: 'alpha', content: '@@exclude_keys gamma\nnegative' },
            ],
            [{ role: 'user', data: 'alpha beta gamma' }],
        )

        await expect(activePrompts()).resolves.toEqual(['selective'])
    })

    test('makes recursive prompts visible in activation order unless recursive search is disabled', async () => {
        setLorebook(
            [
                { key: 'alpha', content: 'beta' },
                { key: 'beta', content: 'recursive' },
                { key: 'beta', content: '@@no_recursive_search\nnot-recursive' },
            ],
            [{ role: 'user', data: 'alpha' }],
        )

        await expect(activePrompts()).resolves.toEqual(['recursive', 'beta'])
    })

    test('applies priority before token budget and insertion order afterward', async () => {
        setLorebook(
            [
                { key: 'alpha', content: '@@priority 10\nAAAA', insertorder: 1 },
                { key: 'alpha', content: '@@priority 0\nBBBB', insertorder: 200 },
            ],
            [{ role: 'user', data: 'alpha' }],
            4,
        )

        await expect(activePrompts()).resolves.toEqual(['AAAA'])
    })

    test('preserves slash regex parsing and matches repeated global and sticky tests', async () => {
        setLorebook(
            [
                { key: '/foo/g', useRegex: true, content: 'global-one' },
                { key: '/foo/g', useRegex: true, content: 'global-two' },
                { key: '/foo/y', useRegex: true, content: 'sticky-one' },
                { key: '/foo/y', useRegex: true, content: 'sticky-two' },
                { key: '/foo/z', useRegex: true, content: 'invalid' },
                { key: 'alpha', content: 'after-invalid' },
            ],
            [{ role: 'user', data: '/foo alpha' }],
        )

        await expect(activePrompts()).resolves.toEqual([
            'after-invalid',
            'sticky-two',
            'sticky-one',
            'global-two',
            'global-one',
        ])
    })

    test('normalizes each depth view once', async () => {
        setLorebook([
            { key: 'alpha', content: 'one' },
            { key: 'alpha', content: 'two' },
            { key: 'alpha', content: 'three' },
        ], [{ role: 'user', data: 'ALPHA' }])
        const lowerCase = vi.spyOn(String.prototype, 'toLocaleLowerCase')

        await expect(activePrompts()).resolves.toEqual(['three', 'two', 'one'])
        expect(lowerCase).toHaveBeenCalledTimes(11)
    })

    test('compiles each exact slash regex once per invocation', async () => {
        setLorebook([
            { key: '/foo/g', useRegex: true, content: 'one' },
            { key: '/foo/g', useRegex: true, content: 'two' },
            { key: '/foo/g', useRegex: true, content: 'three' },
        ], [{ role: 'user', data: '/foo' }])
        const NativeRegExp = globalThis.RegExp
        let constructorCalls = 0
        vi.stubGlobal('RegExp', new Proxy(NativeRegExp, {
            construct(target, args, newTarget) {
                constructorCalls += 1
                return Reflect.construct(target, args, newTarget)
            },
        }))

        await expect(activePrompts()).resolves.toEqual(['three', 'two', 'one'])
        expect(constructorCalls).toBe(1)
    })

    test('reads lore depth from the pinned operation instead of the live DBState array', async () => {
        setLorebook(
            [{ key: 'pinned', content: 'matched-pinned-history' }],
            [{ role: 'user', data: 'live value does not match' }],
        )
        DBState.db.characters[0].loreSettings.scanDepth = 2
        const conversation: Chat = {
            id: 'conversation-pinned',
            name: 'Pinned',
            note: '',
            localLore: [],
            message: [
                { role: 'user', data: 'older' },
                { role: 'char', data: 'pinned' },
            ],
        }
        const session = new ActiveConversationSession({
            characterId: 'character-pinned',
            conversationId: 'conversation-pinned',
            conversation,
            storeRevision: 8,
        })
        const readLatest = vi.spyOn(session, 'readLatest')
        const operation = beginPinnedConversationHistoryOperation(session)

        try {
            const result = await loadLoreBookV3Prompt(operation)
            expect(result.actives.map((active) => active.prompt)).toEqual([
                'matched-pinned-history',
            ])
            expect(readLatest).toHaveBeenCalledTimes(1)
            expect(readLatest).toHaveBeenCalledWith(2)
            expect(session.pinCount('prompt')).toBe(1)
        } finally {
            operation.dispose()
        }
        expect(session.pinCount('prompt')).toBe(0)
    })
})
