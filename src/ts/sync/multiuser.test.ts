import { beforeEach, describe, expect, it, vi } from 'vitest'

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

import { installReceivedCharacter, normalizeIncomingChat } from './multiuser'

const buildDatabase = () => ({
    characters: [
        {
            chaId: 'local-char',
            chats: [{ message: [], note: '', name: 'Local', localLore: [], id: 'local-chat' }],
            chatPage: 0,
        },
    ],
    characterOrder: [],
}) as any

const buildIncoming = (overrides: any = {}) => ({
    chaId: 'host-char',
    name: 'Host',
    chats: [{ message: [], note: '', name: 'Host Chat', localLore: [] }],
    chatPage: 3,
    ...overrides,
}) as any

describe('multiuser character install', () => {
    beforeEach(() => {
        mocks.nextId = 0
        vi.clearAllMocks()
    })

    it('assigns ids to received chats and keeps the temp sentinel', () => {
        const db = buildDatabase()
        const incoming = buildIncoming()

        const index = installReceivedCharacter(db, incoming)

        expect(index).toBe(1)
        expect(db.characters[1].chaId).toBe('§temp')
        expect(db.characters[1].chatPage).toBe(0)
        expect(db.characters[1].chats.every((chat: any) => chat.id)).toBe(true)
    })

    it('reassigns received chat ids that collide with existing ones', () => {
        const db = buildDatabase()
        const incoming = buildIncoming({
            chats: [{ message: [], note: '', name: 'Host Chat', localLore: [], id: 'local-chat' }],
        })

        installReceivedCharacter(db, incoming)

        const ids = db.characters.flatMap((character: any) =>
            character.chats.map((chat: any) => chat.id),
        )
        expect(new Set(ids).size).toBe(ids.length)
        expect(db.characters[0].chats[0].id).toBe('local-chat')
    })

    it('replaces an existing temp character instead of stacking a new one', () => {
        const db = buildDatabase()
        installReceivedCharacter(db, buildIncoming())

        const index = installReceivedCharacter(db, buildIncoming({ name: 'Second' }))

        expect(index).toBe(1)
        expect(db.characters).toHaveLength(2)
        expect(db.characters[1].name).toBe('Second')
    })

    it('drops holes in the received chat list', () => {
        const db = buildDatabase()
        const incoming = buildIncoming({ chats: [undefined] })

        const index = installReceivedCharacter(db, incoming)

        expect(db.characters[index].chats).toEqual([])
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
})
