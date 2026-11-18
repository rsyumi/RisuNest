import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

vi.mock('../stores.svelte', () => ({
    DBState: { db: { characters: [] } },
    selIdState: { selId: -1 },
}))
import { language } from 'src/lang'
import { DBState } from '../stores.svelte'
import type { Database } from '../storage/database.svelte'
import type { SettingContext } from './types'
import { getLabel, getSettingValue, resolveLanguagePath, setSettingValue } from './utils'

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

describe('setting values', () => {
    it('reads and writes the database its context names, which the settings window points at the selected preset', () => {
        const edited = { aiModel: 'edited', ooba: { top_p: 1 } } as unknown as Database
        const ctx = { db: edited } as SettingContext
        expect(getSettingValue({ id: 'model', type: 'text', bindKey: 'aiModel' }, ctx)).toBe('edited')
        expect(getSettingValue({ id: 'top', type: 'number', bindPath: 'ooba.top_p' }, ctx)).toBe(1)
        expect(getSettingValue({ id: 'read', type: 'text', getValue: (db) => db.aiModel }, ctx)).toBe('edited')
        setSettingValue({ id: 'model', type: 'text', bindKey: 'aiModel' }, 'changed', ctx)
        setSettingValue({ id: 'top', type: 'number', bindPath: 'ooba.top_p' }, 0.5, ctx)
        setSettingValue({ id: 'main', type: 'text', setValue: (db, value) => { db.mainPrompt = value } }, 'main', ctx)
        expect(edited).toMatchObject({ aiModel: 'changed', ooba: { top_p: 0.5 }, mainPrompt: 'main' })
        expect(DBState.db).toEqual({ characters: [] })
    })
})
