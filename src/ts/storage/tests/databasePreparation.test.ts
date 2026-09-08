import { describe, expect, it, vi } from 'vitest'

vi.mock('../saveCoordinator', async (importOriginal) => {
    const actual = await importOriginal<typeof import('../saveCoordinator')>()
    return { ...actual, canonicalJson: vi.fn(actual.canonicalJson) }
})

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
import {
    checkCharOrder,
    prepareDatabaseForPersistence,
    prepareDatabaseForBootstrap,
    preparePersistentRootForWorkingSet,
} from '../databasePreparation'
import type { PersistentRoot } from '../persistentDataStore'
import { fixtureDatabase } from './persistentDataFixtures'
import { canonicalJson } from '../saveCoordinator'

function deterministicIds(...ids: string[]): () => string {
    let index = 0
    return () => ids[index++]
}

describe('prepareDatabaseForPersistence', () => {
    it('reports real normalization changes and becomes unchanged after preparation', async () => {
        const input = await prepareDatabaseForPersistence(fixtureDatabase, { now: 1_700_000_000_000 })
        input.formatversion = 4
        input.loreBookToken = 400
        input.characters[0].chaId = ''
        const original = structuredClone(input)
        const options = {
            now: 1_700_000_000_000,
            createId: () => 'synthetic-prepared-id',
        }

        const expected = await prepareDatabaseForPersistence(input, options)
        vi.mocked(canonicalJson).mockClear()
        const prepared = await prepareDatabaseForBootstrap(input, options)
        expect(canonicalJson).toHaveBeenCalledTimes(2)
        expect(prepared.database).toEqual(expected)
        expect(prepared.changed).toBe(true)
        expect(input).toEqual(original)
        expect(prepared.database).not.toBe(input)

        const stable = await prepareDatabaseForBootstrap(
            prepared.database,
            options,
        )
        expect(stable.changed).toBe(false)
        expect(stable.database).toEqual(prepared.database)
        expect(stable.database).not.toBe(prepared.database)
        vi.mocked(canonicalJson).mockClear()
        await prepareDatabaseForPersistence(stable.database, options)
        expect(canonicalJson).toHaveBeenCalledTimes(1)
    })

    it('keeps the bootstrap input intact when detached normalization fails', async () => {
        const input = structuredClone(fixtureDatabase)
        input.characters[0].chaId = ''
        const original = structuredClone(input)
        await expect(
            prepareDatabaseForBootstrap(input, {
                createId: () => {
                    throw new Error('Synthetic preparation failed')
                },
                now: 0,
            }),
        ).rejects.toThrow('Synthetic preparation failed')
        expect(input).toEqual(original)
    })

    it('normalizes root data without treating the catalog order as missing characters', async () => {
        const database = structuredClone(fixtureDatabase)
        database.formatversion = 4
        database.loreBookToken = 400
        database.characterOrder = [
            {
                name: 'Favorites',
                id: 'folder-favorites',
                color: '#ffffff',
                data: ['char-a', 'missing-character'],
            },
            'char-b',
        ]
        const { characters: _characters, botPresets: _botPresets, ...root } = database
        delete (root as Partial<PersistentRoot>).language
        const original = structuredClone(root)

        const prepared = await preparePersistentRootForWorkingSet(root, {
            now: 1_700_000_000_000,
        })

        expect(root).toEqual(original)
        expect(prepared).not.toHaveProperty('characters')
        expect(prepared).not.toHaveProperty('botPresets')
        expect(prepared.formatversion).toBe(5)
        expect(prepared.loreBookToken).toBe(8000)
        expect(prepared.language).toBe('en')
        expect(prepared.characterOrder).toEqual(original.characterOrder)
    })

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

    it('repairs character order in place and preserves retained folder identity', () => {
        const database = structuredClone(fixtureDatabase)
        const folder = {
            name: 'Favorites',
            id: 'folder-favorites',
            color: '#ffffff',
            data: ['char-a', 'missing-character'],
        }
        database.characterOrder = [folder, 'missing-character']
        const order = database.characterOrder

        checkCharOrder(database)

        expect(database.characterOrder).toBe(order)
        expect(database.characterOrder[0]).toBe(folder)
        expect(folder.data).toEqual(['char-a'])
    })

    it('retains a folder emptied by invalid-ID repair until the next call', () => {
        const database = structuredClone(fixtureDatabase)
        const folder = {
            name: 'Missing',
            id: 'folder-missing',
            color: '#ffffff',
            data: ['missing-character'],
        }
        database.characterOrder = [folder]

        checkCharOrder(database)

        expect(database.characterOrder[0]).toBe(folder)
        expect(folder.data).toEqual([])

        checkCharOrder(database)
        expect(database.characterOrder).not.toContain(folder)
    })
})
