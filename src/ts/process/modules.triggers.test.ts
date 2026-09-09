import { describe, expect, it, vi } from 'vitest'
import { derived, get, proxy } from 'svelte/internal/client'
import type { Database, triggerscript } from '../storage/database.svelte'

vi.mock('src/lang', () => ({ language: {} }))
vi.mock('../alert', () => ({}))
vi.mock('../storage/database.svelte', () => ({
    getDatabase: vi.fn(),
    getCurrentCharacter: vi.fn(),
    getCurrentChat: vi.fn(),
}))
vi.mock('../globalApi.svelte', () => ({}))
vi.mock('../util', () => ({ checkPersonaBinded: vi.fn() }))
vi.mock('./lorebook.svelte', () => ({}))
vi.mock('../rpack/rpack_js', () => ({}))
vi.mock('../stores.svelte', () => ({}))
vi.mock('../interchangeability', () => ({}))
vi.mock('../characterCards', () => ({}))
vi.mock('../storage/nativeModuleFileRoute', () => ({}))

import { getDatabase } from '../storage/database.svelte'
import { getModuleTriggers, type RisuModule } from './modules'

let fixtureId = 0

function selectModules(modules: RisuModule[]) {
    // Unique IDs keep each fixture independent of the enabled-module cache.
    const records = proxy(
        modules.map((module) => ({
            ...module,
            id: `synthetic-module-${++fixtureId}`,
        })),
    )
    vi.mocked(getDatabase).mockReturnValue({
        modules: records,
        enabledModules: records.map((module) => module.id),
    } as Database)
    return records
}

function makeTrigger(lowLevelAccess?: boolean): triggerscript {
    return {
        comment: 'Synthetic display trigger',
        type: 'display',
        conditions: [],
        effect: [{ type: 'triggerlua', code: '-- synthetic' }],
        lowLevelAccess,
    }
}

describe('module trigger reads', () => {
    it.each([true, false, undefined])(
        'can derive triggers with module permission %s without mutating stored state',
        (permission) => {
            const records = selectModules([
                {
                    id: '',
                    name: 'Synthetic module',
                    description: '',
                    lowLevelAccess: permission,
                    trigger: [makeTrigger(!permission)],
                },
            ])
            const stored = records[0].trigger![0]
            const before = JSON.stringify(records)
            // This is the Svelte $derived evaluation used by live display input
            // capture, with real state proxies and the real module getter.
            const triggers = derived(getModuleTriggers)
            const result = get(triggers)

            expect(result).toEqual([{ ...stored, lowLevelAccess: permission }])
            expect(result[0]).not.toBe(stored)
            expect(JSON.stringify(records)).toBe(before)
            result[0].lowLevelAccess = permission
            expect(JSON.stringify(records)).toBe(before)
        },
    )

    it('preserves trigger order and observes live trigger and permission edits', () => {
        const records = selectModules([
            { id: '', name: 'Empty', description: '' },
            {
                id: '',
                name: 'First',
                description: '',
                lowLevelAccess: false,
                trigger: [
                    makeTrigger(),
                    { ...makeTrigger(), comment: 'Second' },
                ],
            },
            {
                id: '',
                name: 'Last',
                description: '',
                lowLevelAccess: true,
                trigger: [{ ...makeTrigger(), comment: 'Third' }],
            },
        ])
        const triggers = derived(() =>
            getModuleTriggers().map(({ comment, lowLevelAccess }) => ({
                comment,
                lowLevelAccess,
            })),
        )
        expect(get(triggers)).toEqual([
            { comment: 'Synthetic display trigger', lowLevelAccess: false },
            { comment: 'Second', lowLevelAccess: false },
            { comment: 'Third', lowLevelAccess: true },
        ])

        records[1].lowLevelAccess = true
        records[1].trigger![0].comment = 'Edited'
        expect(get(triggers)).toEqual([
            { comment: 'Edited', lowLevelAccess: true },
            { comment: 'Second', lowLevelAccess: true },
            { comment: 'Third', lowLevelAccess: true },
        ])
        expect(records[1].trigger![0].lowLevelAccess).toBeUndefined()
    })
})
