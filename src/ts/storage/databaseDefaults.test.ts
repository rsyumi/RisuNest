import { describe, expect, it, vi } from 'vitest'

vi.mock('../util', () => ({
    checkNullish: (value: unknown) => value === null || value === undefined,
    decryptBuffer: vi.fn(),
    encryptBuffer: vi.fn(),
    selectSingleFile: vi.fn(),
}))
vi.mock('../alert', () => ({ alertNormal: vi.fn() }))
vi.mock('../gui/colorscheme', () => ({ defaultColorScheme: {} }))
vi.mock('../translator/presets', () => ({ normalizeTranslatorPresetState: vi.fn() }))
vi.mock('../stores.svelte', async () => {
    const { writable } = await import('svelte/store')
    return {
        DBState: { db: { characters: [] } },
        selectedCharID: writable(-1),
        selIdState: { selId: -1 },
    }
})
vi.mock('../model/modellist', () => ({
    LLMFlags: {},
    LLMFormat: { OpenAICompatible: 'openai-compatible' },
    LLMTokenizer: {},
}))
import { normalizeDatabaseDefaults, type Database } from './database.svelte'

describe('RisuNest inlay database defaults', () => {
    it('normalizes the persisted inlay settings to their exact defaults', () => {
        const database = normalizeDatabaseDefaults({ characters: [] } as Database)

        expect(database.risunestInlayFormat).toBe('webp')
        expect(database.risunestInlayWebpQuality).toBe(85)
        expect(database.risunestInlayMaxDimension).toBe(0)
        expect(database.risunestInlaySkipReencode).toBe(false)
    })

    it('clamps and integer-normalizes persisted inlay numbers', () => {
        const database = normalizeDatabaseDefaults({
            characters: [],
            risunestInlayWebpQuality: 101.6,
            risunestInlayMaxDimension: -2.4,
        } as Database)

        expect(database.risunestInlayWebpQuality).toBe(100)
        expect(database.risunestInlayMaxDimension).toBe(0)
    })
})
