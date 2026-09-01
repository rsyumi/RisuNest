import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

vi.mock('../stores.svelte', () => ({
    DBState: { db: { characters: [] } },
    selIdState: { selId: -1 },
}))
import { language } from 'src/lang'
import { getLabel, resolveLanguagePath } from './utils'

describe('setting language resolution', () => {
    let hadRisuNest = false
    let originalRisuNest: unknown

    beforeEach(() => {
        hadRisuNest = Object.prototype.hasOwnProperty.call(language, 'risuNest')
        originalRisuNest = (language as Record<string, unknown>).risuNest
    })

    afterEach(() => {
        if (hadRisuNest) {
            (language as Record<string, unknown>).risuNest = originalRisuNest
        } else {
            delete (language as Record<string, unknown>).risuNest
        }
    })

    it('resolves existing flat language keys', () => {
        expect(resolveLanguagePath('advancedSettings')).toBe(language.advancedSettings)
    })

    it('resolves RisuNest dotted language keys', () => {
        const risuNest = {
            inlay: { format: 'Inlay format' },
        }
        ;(language as Record<string, unknown>).risuNest = risuNest

        expect(resolveLanguagePath('risuNest.inlay.format')).toBe(risuNest.inlay.format)
        expect(getLabel({
            id: 'inlay.format',
            type: 'select',
            labelKey: 'risuNest.inlay.format',
            fallbackLabel: 'Fallback format',
        })).toBe('Inlay format')
    })
})
