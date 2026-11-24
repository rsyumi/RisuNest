import { describe, expect, it, vi } from 'vitest'
import { writable } from 'svelte/store'
import { language } from 'src/lang'
import { customFlagsSummary, customTogglesSummary, listSummary, moduleIntegrationSummary, toolsSummary } from './othersTabSummaries'

// The toggle parser lives in util, whose imports start the app's stores.
vi.mock('src/ts/storage/database.svelte', () => ({}))
vi.mock('src/ts/characters', () => ({}))
vi.mock('src/ts/stores.svelte', () => ({ DBState: { db: {} }, selectedCharID: writable(0) }))
vi.mock('src/lib/UI/PopupList.svelte', () => ({ default: {} }))

const count = (value: number) => language.risuNest.promptOptions.count.replace('{0}', String(value))

describe('others tab summaries', () => {
    it('counts rows and shows nothing for an empty or missing list', () => {
        expect(listSummary([['a', 1], ['b', 2], ['c', 3]])).toBe(count(3))
        expect(listSummary([])).toBe('')
        expect(listSummary(undefined)).toBe('')
    })

    it('counts chosen custom flags only while custom flags are enabled', () => {
        expect(customFlagsSummary(true, [0, 4, 4])).toBe(count(2))
        expect(customFlagsSummary(false, [0, 4])).toBe('')
        expect(customFlagsSummary(true, [])).toBe('')
    })

    it('counts the module IDs entered, without empty pieces', () => {
        expect(moduleIntegrationSummary(' first, ,second ,, third ')).toBe(count(3))
        expect(moduleIntegrationSummary(' , ')).toBe('')
        expect(moduleIntegrationSummary('')).toBe('')
    })

    it('names the enabled tools', () => {
        expect(toolsSummary(['search'])).toBe(language.search)
        expect(toolsSummary([])).toBe('')
    })

    it('counts the toggles that draw a control, not groups, dividers, captions or skipped rows', () => {
        const text = [
            'a=Switch',
            '=Group=group',
            'b=Choice=select=One,Two',
            'c=Field=text\r',
            '==groupEnd',
            '=Section=divider',
            '=Note=caption',
            'd=Notes=textarea',
            '=No key',
            'e=',
            '',
        ].join('\n')
        expect(customTogglesSummary(text)).toBe(count(4))
        expect(customTogglesSummary('=Group=group\n==groupEnd')).toBe('')
        expect(customTogglesSummary(undefined)).toBe('')
    })
})
