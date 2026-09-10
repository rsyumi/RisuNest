import { describe, expect, it, vi } from 'vitest'

vi.mock('../util', () => ({
    checkNullish: (value: unknown) => value === null || value === undefined,
    decryptBuffer: vi.fn(),
    encryptBuffer: vi.fn(),
    selectSingleFile: vi.fn(),
}))
vi.mock('../alert', () => ({ alertNormal: vi.fn() }))
vi.mock('../gui/colorscheme', () => ({ defaultColorScheme: {} }))
vi.mock('../translator/presets', () => ({
    normalizeTranslatorPresetState: vi.fn(),
}))
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

describe('streaming display defaults', () => {
    it('enables compact thoughts by default and preserves an explicit opt-out', () => {
        expect(
            normalizeDatabaseDefaults({ characters: [] } as Database)
                .streamingThoughtMode,
        ).toBe('recent')
        expect(
            normalizeDatabaseDefaults({
                characters: [],
                streamingThoughtMode: 'off',
            } as Database).streamingThoughtMode,
        ).toBe('off')
        expect(
            normalizeDatabaseDefaults({ characters: [] } as Database)
                .streamingDeferDisplayProcessing,
        ).toBe(false)
        expect(
            normalizeDatabaseDefaults({
                characters: [],
                streamingDeferDisplayProcessing: true,
            } as Database).streamingDeferDisplayProcessing,
        ).toBe(true)
    })

    it('does not import the removed RisuNest-only performance setting', () => {
        const database = normalizeDatabaseDefaults({
            characters: [],
            largeChatPerformanceMode: 'strong',
        } as unknown as Database)
        expect(database.streamingDisplayOptimizationMode).toBe('off')
    })
})

describe('RisuNest inlay database defaults', () => {
    it('normalizes the persisted inlay settings to their exact defaults', () => {
        const database = normalizeDatabaseDefaults({
            characters: [],
        } as Database)

        expect(database.risunestInlayFormat).toBe('webp')
        expect(database.risunestInlayWebpQuality).toBe(85)
        expect(database.risunestInlayMaxDimension).toBe(0)
        expect(database.risunestInlaySkipReencode).toBe(true)
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

    it.each([
        [Number.NaN, 0],
        [-1, 0],
        [12.6, 13],
        [4_294_967_296, 4_294_967_295],
        [Number.MAX_SAFE_INTEGER, 4_294_967_295],
    ])(
        'normalizes persisted maximum dimension %s into the native u32 range',
        (input, expected) => {
            const database = normalizeDatabaseDefaults({
                characters: [],
                risunestInlayMaxDimension: input,
            } as Database)

            expect(database.risunestInlayMaxDimension).toBe(expected)
        },
    )
})
