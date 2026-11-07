import { beforeEach, describe, expect, it, vi } from 'vitest'
import { get as getStore, writable } from 'svelte/store'
import type { Database, triggerscript } from '../storage/database.svelte'

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
vi.mock('../stores.svelte', () => ({
    HideIconStore: writable(false),
    moduleBackgroundEmbedding: writable(''),
    ReloadGUIPointer: writable(0),
}))
vi.mock('../interchangeability', () => ({}))
vi.mock('../characterCards', () => ({}))

import { getCurrentCharacter, getCurrentChat, getDatabase } from '../storage/database.svelte'
import { checkPersonaBinded } from '../util'
import { HideIconStore, moduleBackgroundEmbedding } from '../stores.svelte'
import { getModules, getModuleToggles, getModuleTriggers, moduleUpdate, type RisuModule } from './modules'

beforeEach(() => {
    vi.mocked(getCurrentCharacter).mockReturnValue(undefined)
    vi.mocked(getCurrentChat).mockReturnValue(undefined)
    vi.mocked(checkPersonaBinded).mockReturnValue(null)
})

let fixtureId = 0

function selectModules(modules: RisuModule[]) {
    const records = $state(
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
            const triggers = $derived.by(getModuleTriggers)
            const result = triggers

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
        const triggers = $derived(
            getModuleTriggers().map(({ comment, lowLevelAccess }) => ({
                comment,
                lowLevelAccess,
            })),
        )
        expect(triggers).toEqual([
            { comment: 'Synthetic display trigger', lowLevelAccess: false },
            { comment: 'Second', lowLevelAccess: false },
            { comment: 'Third', lowLevelAccess: true },
        ])

        records[1].lowLevelAccess = true
        records[1].trigger![0].comment = 'Edited'
        expect(triggers).toEqual([
            { comment: 'Edited', lowLevelAccess: true },
            { comment: 'Second', lowLevelAccess: true },
            { comment: 'Third', lowLevelAccess: true },
        ])
        expect(records[1].trigger![0].lowLevelAccess).toBeUndefined()
    })
})

describe('live module selection', () => {
    it('allows the initial empty working set before database defaults are installed', () => {
        vi.mocked(getDatabase).mockReturnValue({} as Database)
        expect(getModules()).toEqual([])
        expect(() => moduleUpdate()).not.toThrow()
    })

    it('observes replacement, deletion and namespace edits without changing enabled IDs', () => {
        const db = $state({
            modules: [{ id: 'module', name: 'Original', description: '', namespace: 'shared', customModuleToggle: 'old=Old' }],
            enabledModules: ['shared'],
        })
        vi.mocked(getDatabase).mockReturnValue(db as Database)
        const toggles = $derived.by(getModuleToggles)
        expect(toggles).toContain('old=Old')
        db.modules[0] = { ...db.modules[0], customModuleToggle: 'new=New' }
        expect(toggles).toContain('new=New')
        db.modules[0].namespace = 'other'
        expect(toggles).toBe('')
        db.enabledModules = ['module']
        expect(toggles).toContain('new=New')
        db.modules.splice(0, 1)
        expect(toggles).toBe('')
    })

    it('keeps different ID lists distinct even when their hyphen-joined strings match', () => {
        const modules = ['a-b', 'c', 'a', 'b-c'].map(id => ({ id, name: id, description: '' }))
        const db = { modules, enabledModules: ['a-b', 'c'] }
        vi.mocked(getDatabase).mockReturnValue(db as Database)
        expect(getModules().map(m => m.id)).toEqual(['a-b', 'c'])
        db.enabledModules = ['a', 'b-c']
        expect(getModules().map(m => m.id)).toEqual(['a', 'b-c'])
    })

    it('includes the bound persona module and switches same-ID embedded modules immediately', () => {
        vi.mocked(getDatabase).mockReturnValue({ modules: [], enabledModules: [] } as unknown as Database)
        const persona = { id: 'persona-a', name: 'A', icon: '', personaPrompt: '', embeddedModule: {
            id: '$embedded', name: 'A module', description: '', customModuleToggle: 'a=A',
        } }
        vi.mocked(checkPersonaBinded).mockReturnValue(persona)
        expect(getModuleToggles()).toContain('a=A')
        vi.mocked(checkPersonaBinded).mockReturnValue({ ...persona, id: 'persona-b', embeddedModule: {
            ...persona.embeddedModule, customModuleToggle: 'b=B',
        } })
        expect(getModuleToggles()).toContain('b=B')
        expect(getModuleToggles()).not.toContain('a=A')
        vi.mocked(checkPersonaBinded).mockReturnValue(null)
        expect(getModules()).toEqual([])
    })

    it('clears module background and icon overrides when the module is disabled', () => {
        const db = { modules: [{ id: 'background', name: 'Background', description: '',
            backgroundEmbedding: '<p>Synthetic</p>', hideIcon: true }], enabledModules: ['background'] }
        vi.mocked(getDatabase).mockReturnValue(db as Database)
        moduleUpdate()
        expect(getStore(moduleBackgroundEmbedding)).toContain('<p>Synthetic</p>')
        expect(getStore(HideIconStore)).toBe(true)
        db.enabledModules = []
        moduleUpdate()
        expect(getStore(moduleBackgroundEmbedding)).toBe('')
        expect(getStore(HideIconStore)).toBeFalsy()
    })
})
