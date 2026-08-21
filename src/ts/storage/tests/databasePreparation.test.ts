import { describe, expect, it, vi } from 'vitest'

vi.mock('../../util', () => ({
    checkNullish: (value: unknown) => value === null || value === undefined,
    decryptBuffer: vi.fn(),
    encryptBuffer: vi.fn(),
    selectSingleFile: vi.fn(),
}))
vi.mock('../../alert', () => ({
    alertConfirm: vi.fn(async () => false),
    alertError: vi.fn(),
    alertMd: vi.fn(),
    alertNormal: vi.fn(),
    waitAlert: vi.fn(async () => undefined),
}))
vi.mock('../../process/memory/hypav3', () => ({
    createHypaV3Preset: (name: string, settings: unknown) => ({ name, settings }),
}))
vi.mock('../../gui/colorscheme', () => ({
    defaultColorScheme: {
        bgcolor: '#282a36',
        darkbg: '#21222c',
        borderc: '#44475a',
        selected: '#44475a',
        draculared: '#ff5555',
        textcolor: '#f8f8f2',
        textcolor2: '#6272a4',
        darkBorderc: '#282a36',
        darkbutton: '#282a36',
        type: 'dark',
    },
}))
vi.mock('../../translator/presets', () => ({
    normalizeTranslatorPresetState: vi.fn(),
}))
vi.mock('../../stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        DBState: { db: { characters: [] } },
        selectedCharID: writable(-1),
    }
})
vi.mock('../../model/modellist', () => ({
    LLMFlags: {},
    LLMFormat: {
        Ollama: 'ollama',
        OpenAICompatible: 'openai-compatible',
    },
    LLMTokenizer: {},
}))
import type { Database } from '../database.svelte'
import { prepareDatabaseForPersistence } from '../databasePreparation'
import { fixtureDatabase } from './persistentDataFixtures'

function deterministicIds(...ids: string[]): () => string {
    let index = 0
    return () => ids[index++]
}

describe('prepareDatabaseForPersistence', () => {
    it('normalizes a detached database without changing the input', async () => {
        const input = structuredClone(fixtureDatabase)
        input.formatversion = 4
        input.loreBookToken = 400
        delete (input as Partial<Database>).language
        const original = structuredClone(input)

        const prepared = await prepareDatabaseForPersistence(input, { now: 1_700_000_000_000 })

        expect(input).toEqual(original)
        expect(prepared).not.toBe(input)
        expect(prepared.formatversion).toBe(5)
        expect(prepared.loreBookToken).toBe(8000)
        expect(prepared.language).toBe('en')
    })

    it('assigns unique character and chat IDs from one deterministic namespace', async () => {
        const input = structuredClone(fixtureDatabase)
        input.characters[0].chaId = ''
        input.characters[0].chats[0].id = 'shared-id'
        input.characters[1].chaId = 'shared-id'
        input.characters[1].chats[0].id = 'shared-id'
        input.characters[1].chats[1].id = ''

        const prepared = await prepareDatabaseForPersistence(input, {
            createId: deterministicIds('new-character', 'new-character-2', 'new-chat', 'new-chat-2'),
            now: 0,
        })

        const ids = prepared.characters.flatMap((character) => [
            character.chaId,
            ...character.chats.map((chat) => chat.id),
        ])
        expect(ids).toEqual([
            'new-character',
            'shared-id',
            'new-character-2',
            'new-chat',
            'new-chat-2',
            'char-c',
            'conv-trash',
        ])
        expect(new Set(ids).size).toBe(ids.length)
    })

    it('repairs character order after final IDs while retaining valid folders', async () => {
        const input = structuredClone(fixtureDatabase)
        input.characters[0].chaId = ''
        input.characterOrder = [
            {
                name: 'Favorites',
                id: 'folder-favorites',
                color: '#ffffff',
                data: ['char-a', 'missing-character'],
            },
            'missing-character',
            'char-c',
        ]

        const prepared = await prepareDatabaseForPersistence(input, {
            createId: deterministicIds('char-beta'),
            now: 1_700_000_000_000,
        })

        expect(prepared.characterOrder).toEqual([
            { name: 'Favorites', id: 'folder-favorites', color: '#ffffff', data: ['char-a'] },
            'char-beta',
        ])
        expect(input.characterOrder).toEqual([
            {
                name: 'Favorites',
                id: 'folder-favorites',
                color: '#ffffff',
                data: ['char-a', 'missing-character'],
            },
            'missing-character',
            'char-c',
        ])
    })

    it('leaves the input unchanged when preparation fails', async () => {
        const input = structuredClone(fixtureDatabase)
        input.characters[0].chaId = ''
        const original = structuredClone(input)

        await expect(
            prepareDatabaseForPersistence(input, {
                createId: () => {
                    throw new Error('ID allocation failed')
                },
                now: 0,
            }),
        ).rejects.toThrow('ID allocation failed')
        expect(input).toEqual(original)
    })
})
