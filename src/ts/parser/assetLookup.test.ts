import { describe, expect, it, vi } from 'vitest'
import {
    createAssetLookupIndex,
    getAssetDistance,
    resolveAdditionalAsset,
    resolveEmotionAsset,
} from './assetLookup'

describe('asset lookup index', () => {
    it('preserves character, module, and emotion exact lookup behavior', () => {
        const index = createAssetLookupIndex({
            characterAssets: [
                ['Hero', 'char-1.png', 'png'],
                ['Hero', 'char-2.png', 'png'],
            ],
            moduleAssets: [
                ['Hero', 'module.png', 'png'],
                ['Theme', 'theme.mp3', 'mp3'],
            ],
            emotionAssets: [['Smile', 'smile.png']],
        })

        expect(resolveAdditionalAsset(index, 'HERO', 4)?.srcPaths).toEqual([
            'char-1.png',
            'char-2.png',
            'module.png',
        ])
        expect(resolveAdditionalAsset(index, 'theme', 4)).toEqual({
            srcPaths: ['theme.mp3'],
            ext: 'mp3',
        })
        expect(resolveEmotionAsset(index, 'SMILE')).toEqual({ srcPaths: ['smile.png'] })
    })

    it('preserves legacy fuzzy normalization, threshold, and original-order tie breaking', () => {
        const index = createAssetLookupIndex({
            characterAssets: [
                ['happy-face.png', 'happy.png', 'png'],
                ['cot', 'first.png', 'png'],
                ['cut', 'second.png', 'png'],
            ],
            moduleAssets: [['module-only', 'module.png', 'png']],
            emotionAssets: [],
        })

        expect(resolveAdditionalAsset(index, 'happy_face', 0)?.srcPaths).toEqual(['happy.png'])
        expect(resolveAdditionalAsset(index, 'cat', 1)?.srcPaths).toEqual(['first.png'])
        expect(resolveAdditionalAsset(index, 'module-onlx', 2)).toBeNull()
        expect(resolveAdditionalAsset(index, 'unrelated', 1)).toBeNull()
    })

    it('finds the legacy winner even when lexical neighbors would displace it from a fixed window', () => {
        const characterAssets = [
            ['baaaaaaaaaa', 'winner.png', 'png'],
            ...Array.from({ length: 100 }, (_, index) => [
                `aaaaaaaaaa${index.toString().padStart(3, '0')}`,
                `decoy-${index}.png`,
                'png',
            ]),
        ]
        const lookup = createAssetLookupIndex({
            characterAssets,
            moduleAssets: [],
            emotionAssets: [],
        })

        expect(resolveAdditionalAsset(lookup, 'aaaaaaaaaa', 1)?.srcPaths).toEqual(['winner.png'])
    })

    it('preserves combined character then module extension insertion order', () => {
        const lookup = createAssetLookupIndex({
            characterAssets: [['Shared', 'character.png', 'png']],
            moduleAssets: [
                ['Shared', 'ignored.webp', 'webp'],
                ['Shared', 'module.png', 'png'],
            ],
            emotionAssets: [],
        })

        expect(resolveAdditionalAsset(lookup, 'shared', 0)).toEqual({
            srcPaths: ['character.png', 'module.png'],
            ext: 'png',
        })
    })

    it('preserves legacy multi-extension normalization', () => {
        const lookup = createAssetLookupIndex({
            characterAssets: [['portrait.jpg.png', 'portrait.png', 'png']],
            moduleAssets: [],
            emotionAssets: [],
        })

        expect(resolveAdditionalAsset(lookup, 'portrait', 0)?.srcPaths).toEqual(['portrait.png'])
    })

    it('uses safe length pruning without changing equal-distance or miss results', () => {
        const lookup = createAssetLookupIndex({
            characterAssets: [
                ['cot', 'first.png', 'png'],
                ['cut', 'second.png', 'png'],
                ...Array.from({ length: 1_000 }, (_, index) => [
                    `asset-${index.toString().padStart(5, '0')}-far-too-long`,
                    `${index}.png`,
                    'png',
                ]),
            ],
            moduleAssets: [],
            emotionAssets: [],
        })
        const distance = vi.fn(getAssetDistance)

        expect(resolveAdditionalAsset(lookup, 'cat', 1, distance)?.srcPaths).toEqual(['first.png'])
        expect(distance.mock.calls.length).toBe(2)
        expect(resolveAdditionalAsset(lookup, 'missing', 1)).toBeNull()
    })

    it('indexes fuzzy candidates by normalized length and only compares threshold-valid buckets', () => {
        const lookup = createAssetLookupIndex({
            characterAssets: [
                ['cat', 'length-3.png', 'png'],
                ['four', 'length-4.png', 'png'],
                ['fives', 'length-5.png', 'png'],
                ...Array.from({ length: 1_000 }, (_, index) => [
                    `far-too-long-${index.toString().padStart(5, '0')}`,
                    `long-${index}.png`,
                    'png',
                ]),
            ],
            moduleAssets: [],
            emotionAssets: [],
        })
        const distance = vi.fn(getAssetDistance)

        expect([...lookup.characterEntriesByLength.keys()].slice(0, 3)).toEqual([3, 4, 5])
        expect(resolveAdditionalAsset(lookup, 'coat', 1, distance)?.srcPaths).toEqual(['length-3.png'])
        expect(distance).toHaveBeenCalledTimes(3)
    })

    it('preserves the legacy canonical-name alias overwrite after a fuzzy match', () => {
        const lookup = createAssetLookupIndex({
            characterAssets: [
                ['hero.png', 'first.png', 'png'],
                ['hero.png', 'second.png', 'png'],
            ],
            moduleAssets: [],
            emotionAssets: [],
        })

        expect(resolveAdditionalAsset(lookup, 'hero', 0)?.srcPaths).toEqual(['first.png'])
        expect(resolveAdditionalAsset(lookup, 'hero.png', 0)?.srcPaths).toEqual(['first.png'])
    })

    it('searches integer length buckets within a fractional distance threshold', () => {
        const lookup = createAssetLookupIndex({
            characterAssets: [['cat', 'cat.png', 'png']],
            moduleAssets: [],
            emotionAssets: [],
        })

        expect(resolveAdditionalAsset(lookup, 'cot', 1.5)?.srcPaths).toEqual(['cat.png'])
    })

    it('matches an exhaustive original-order scan for fractional thresholds', () => {
        const characterAssets = [
            ['ab', 'length-2.png', 'png'],
            ['abcd', 'length-4-first.png', 'png'],
            ['abce', 'length-4-second.png', 'png'],
            ['abcdef', 'length-6.png', 'png'],
        ]
        const cases = [
            { query: 'abc', threshold: 0.5, expected: null },
            { query: 'abc', threshold: 1.5, expected: 'length-2.png' },
            { query: 'abcde', threshold: 1.5, expected: 'length-4-first.png' },
            { query: 'abcde', threshold: 2.5, expected: 'length-4-first.png' },
        ]

        for (const { query, threshold, expected } of cases) {
            const lookup = createAssetLookupIndex({ characterAssets, moduleAssets: [], emotionAssets: [] })
            expect(resolveAdditionalAsset(lookup, query, threshold)?.srcPaths[0] ?? null).toBe(expected)
        }
    })
})
