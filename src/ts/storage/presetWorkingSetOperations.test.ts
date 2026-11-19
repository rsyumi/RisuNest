import { beforeEach, describe, expect, it, vi } from 'vitest'
import { DBState } from '../stores.svelte'
import { readPresetBodies, type Database, type botPreset } from './database.svelte'
import {
    createPresetWorkingSetController,
    capturePresetNames,
    readPersistentPresetBody,
    readPersistentPresetBodies,
} from './presetWorkingSetOperations'

vi.mock('../util', () => ({
    checkNullish: (value: unknown) => value === null || value === undefined,
    decryptBuffer: vi.fn(), encryptBuffer: vi.fn(), selectSingleFile: vi.fn(),
}))
vi.mock('../alert', () => ({ alertNormal: vi.fn() }))
vi.mock('../gui/colorscheme', () => ({ defaultColorScheme: {} }))
vi.mock('../translator/presets', () => ({ normalizeTranslatorPresetState: vi.fn() }))
vi.mock('../model/modellist', () => ({ LLMFlags: {}, LLMFormat: {}, LLMTokenizer: {} }))
vi.mock('../process/modules', () => ({ moduleUpdate: vi.fn() }))
vi.mock('../process/scripts', () => ({ resetScriptCache: vi.fn() }))
vi.mock('../parser/parser.svelte', async () => (await import('../parser/parser.testSupport')).parserStubModule())
vi.mock('../process/memory/hypav3', () => ({ createHypaV3Preset: vi.fn() }))
const runtime = vi.hoisted(() => ({
    mutatePersistentPresets: vi.fn(),
    store: { queryPresets: vi.fn(), readPreset: vi.fn() },
    revision: 1,
}))
vi.mock('./persistentDataRuntime.svelte', () => ({
    mutatePersistentPresets: runtime.mutatePersistentPresets,
    getPersistentDataRuntime: () => runtime,
}))

function preset(name: string, mainPrompt: string): botPreset {
    return { name, mainPrompt } as botPreset
}

describe('store-backed preset operations', () => {
    let live: Database
    let persisted: botPreset[]
    let failCommit: boolean
    let controller: ReturnType<typeof createPresetWorkingSetController>

    beforeEach(() => {
        live = {
            botPresetsId: 0,
            botPresets: [preset('First', 'catalog stub'), preset('Second', 'catalog stub')],
            mainPrompt: 'edited active first',
            characters: [],
        } as unknown as Database
        persisted = [preset('First', 'stored first'), preset('Second', 'stored second')]
        failCommit = false
        controller = createPresetWorkingSetController({
            getDatabase: () => live,
            captureCurrentPreset: (database) => ({
                ...persisted[database.botPresetsId],
                name: database.botPresets[database.botPresetsId].name,
                mainPrompt: database.mainPrompt,
            }),
            applyPreset: (root, selected) => {
                root.mainPrompt = selected.mainPrompt
            },
            mutatePersistentPresets: vi.fn(async (_reason, mutate) => {
                const { characters: _characters, botPresets: _botPresets, ...root } = structuredClone(live)
                const state = { root, presets: structuredClone(persisted) }
                await mutate(state)
                if (failCommit) throw new Error('commit failed')
                persisted = state.presets
                Object.assign(live, state.root)
                live.botPresets = persisted.map((item, index) => index === live.botPresetsId
                    ? structuredClone(item)
                    : { name: item.name } as botPreset)
            }),
        })
    })

    it('validates a repeated selection without applying or persisting over pending edits', async () => {
        const before = live.botPresets
        await controller.changeToPreset(0, false)
        expect(live.botPresets).toBe(before)
        expect(live.mainPrompt).toBe('edited active first')
        expect(persisted[0].mainPrompt).toBe('stored first')
        await expect(controller.changeToPreset(0, true, ['Wrong', 'Second'])).rejects.toThrow('Preset list changed')
        await controller.saveCurrentPreset()
        expect(persisted[0].mainPrompt).toBe('edited active first')
    })

    it('saves the active body and hydrates the selected inactive body before returning', async () => {
        await controller.changeToPreset(1)

        expect(persisted[0].mainPrompt).toBe('edited active first')
        expect(live.botPresetsId).toBe(1)
        expect(live.mainPrompt).toBe('stored second')
        expect(live.botPresets[0]).toEqual({ name: 'First' })
        expect(live.botPresets[1].mainPrompt).toBe('stored second')
    })

    it('adds, renames, moves, and removes presets through complete store-backed mutations', async () => {
        const addedIndex = await controller.addPreset(preset('Third', 'three'), true)
        expect(addedIndex).toBe(2)
        expect(live.botPresetsId).toBe(2)

        await controller.renamePreset(0, 'Renamed first')
        await controller.movePreset(0, 3)
        expect(persisted.map((item) => item.name)).toEqual([
            'Second',
            'Third',
            'Renamed first',
        ])

        await controller.removePreset(1)
        expect(persisted.map((item) => item.name)).toEqual(['Second', 'Renamed first'])
        expect(live.botPresetsId).toBe(0)
        expect(live.mainPrompt).toBe('stored second')
    })

    it('clears a removed plugin provider from every stored body, behind catalog stubs too', async () => {
        persisted = [
            { ...preset('First', 'stored first'), currentPluginProvider: 'removed' },
            { ...preset('Second', 'stored second'), currentPluginProvider: 'removed' },
            { ...preset('Third', 'stored third'), currentPluginProvider: 'kept' },
        ]
        live.botPresets.push(preset('Third', 'catalog stub'))
        live.currentPluginProvider = 'removed'

        await controller.clearPluginProvider(['removed'])

        expect(persisted.map((item) => item.currentPluginProvider)).toEqual(['', '', 'kept'])
        expect(live.currentPluginProvider).toBe('')
    })

    it('copies an inactive full body instead of its catalog stub', async () => {
        await controller.copyPreset(1)

        expect(persisted[2]).toEqual({ ...preset('Second Copy', 'stored second'), id: expect.stringMatching(/^[0-9a-f-]{36}$/) })
        expect(persisted[2].id).not.toBe(persisted[1].id)
    })

    it('assigns a fresh identity when adding a file preset with an existing ID', async () => {
        const incoming = { ...preset('Imported', 'imported'), id: 'file-id' }
        const added = await controller.addPreset(incoming)
        expect(persisted[added].id).toMatch(/^[0-9a-f-]{36}$/)
        expect(persisted[added].id).not.toBe('file-id')
        expect(incoming.id).toBe('file-id')
    })

    it('does not alter the live working set when persistence fails before publication', async () => {
        const before = JSON.stringify(live)
        failCommit = true

        await expect(controller.changeToPreset(1)).rejects.toThrow('commit failed')

        expect(JSON.stringify(live)).toBe(before)
    })

    it('reads a complete inactive body for export from one revision', async () => {
        const store = {
            queryPresets: vi.fn(async () => ({
                revision: 9,
                items: [
                    { id: 'preset-a', configuredIndex: 0, name: 'Active' },
                    { id: 'preset-b', configuredIndex: 1, name: 'Inactive' },
                ],
            })),
            readPreset: vi.fn(async () => ({
                revision: 9,
                value: preset('Inactive', 'complete inactive export body'),
            })),
        }

        const exported = await readPersistentPresetBody(store, 9, 1)

        expect(store.readPreset).toHaveBeenCalledWith('preset-b')
        expect(exported.mainPrompt).toBe('complete inactive export body')
    })

    it('reads compared preset bodies from one catalog revision', async () => {
        const store = {
            queryPresets: vi.fn(async () => ({
                revision: 11,
                items: [
                    { id: 'preset-a', configuredIndex: 0, name: 'First' },
                    { id: 'preset-b', configuredIndex: 1, name: 'Second' },
                ],
            })),
            readPreset: vi.fn(async (id: string) => ({
                revision: 11,
                value: preset(id, `complete ${id}`),
            })),
        }

        const compared = await readPersistentPresetBodies(store, 11, [0, 1])

        expect(store.queryPresets).toHaveBeenCalledOnce()
        expect(compared.map((item) => item.mainPrompt)).toEqual([
            'complete preset-a',
            'complete preset-b',
        ])
    })
})

it('saves only mirror fields into the effective stable record and preserves protected and opaque copies', async () => {
    const live = {
        botPresetsId: 0, botPresets: [{ id: 'a' }, { id: 'b' }], doNotChangeFallbackModels: true,
    } as unknown as Database
    const stored = [{ id: 'a', mainPrompt: 'A' }, {
        id: 'b', mainPrompt: 'B', openAIKey: 'stored opaque', extension: 'stored extension',
        fallbackModels: { model: ['stored protected'] }, autoSuggestClean: true,
    }] as unknown as botPreset[]
    const controller = createPresetWorkingSetController({
        getDatabase: () => live,
        getEffectivePresetId: () => 'b',
        captureCurrentPreset: () => ({
            id: 'b', mainPrompt: 'edited B', openAIKey: 'stale opaque', extension: 'stale extension',
            fallbackModels: { model: ['stale protected'] },
        }) as unknown as botPreset,
        applyPreset: () => {},
        mutatePersistentPresets: async (_reason, mutate) => { await mutate({ root: {} as any, presets: stored }) },
    })
    await controller.saveCurrentPreset()
    expect(stored[0].mainPrompt).toBe('A')
    expect(stored[1]).toMatchObject({ id: 'b', mainPrompt: 'edited B', openAIKey: 'stored opaque', extension: 'stored extension', fallbackModels: { model: ['stored protected'] } })
    expect(stored[1]).not.toHaveProperty('autoSuggestClean')
})

it('rejects an originally selected index after an earlier queued reorder publishes', async () => {
    let live = { botPresetsId: 0, botPresets: ['A', 'B', 'C'].map(name => preset(name, name)) } as Database
    let queue = Promise.resolve()
    const controller = createPresetWorkingSetController({
        getDatabase: () => live,
        captureCurrentPreset: () => null,
        applyPreset: () => {},
        mutatePersistentPresets: (_reason, mutate) => {
            const task = queue.then(async () => {
                const state = { root: { botPresetsId: live.botPresetsId } as any, presets: structuredClone(live.botPresets) }
                await mutate(state)
                live = { ...live, ...state.root, botPresets: state.presets }
            })
            queue = task.catch(() => {})
            return task
        },
    })
    const moved = controller.movePreset(0, 3)
    const removed = controller.removePreset(1)
    await moved
    await expect(removed).rejects.toThrow('Preset list changed')
    expect(live.botPresets.map(item => item.name)).toEqual(['B', 'C', 'A'])
})

it.each(['remove', 'copy'] as const)('rejects queued %s after a same-name reorder without changing the wrong body', async operation => {
    let live = { botPresetsId: 0, botPresets: ['A', 'B', 'C'].map(body => preset('X', body)) } as Database
    let queue = Promise.resolve()
    const controller = createPresetWorkingSetController({
        getDatabase: () => live,
        captureCurrentPreset: () => null,
        applyPreset: () => {},
        mutatePersistentPresets: (_reason, mutate) => {
            const task = queue.then(async () => {
                const state = { root: { botPresetsId: live.botPresetsId } as any, presets: structuredClone(live.botPresets) }
                await mutate(state)
                live = { ...live, ...state.root, botPresets: state.presets }
            })
            queue = task.catch(() => {})
            return task
        },
    })
    const moved = controller.movePreset(0, 3)
    const pending = operation === 'remove' ? controller.removePreset(1) : controller.copyPreset(1)
    await moved
    await expect(pending).rejects.toThrow('Preset list changed')
    expect(live.botPresets.map(item => item.mainPrompt)).toEqual(['B', 'C', 'A'])
    await controller.copyPreset(0, ['X', 'X', 'X'])
    expect(live.botPresets[3]).toEqual({ ...preset('X Copy', 'B'), id: expect.stringMatching(/^[0-9a-f-]{36}$/) })
})

it('keeps the selection when another preset is removed and names the selection for the icon while another preset is applied', async () => {
    let chainPick: string | null = 'b'
    const live = {
        botPresetsId: 2,
        botPresets: ['a', 'b', 'c', 'd', 'e'].map(id => ({ ...preset(id.toUpperCase(), id), id })),
    } as unknown as Database
    const controller = createPresetWorkingSetController({
        getDatabase: () => live,
        getEffectivePresetId: () => chainPick ?? live.botPresets[live.botPresetsId].id,
        getEffectivePresetOverride: () => chainPick,
        clearEffectivePresetOverride: () => { chainPick = null },
        captureCurrentPreset: () => null,
        applyPreset: (root, selected) => { (root as Database).mainPrompt = selected.mainPrompt },
        mutatePersistentPresets: async (_reason, mutate) => {
            const state = { root: { botPresetsId: live.botPresetsId } as any, presets: structuredClone(live.botPresets) }
            await mutate(state)
            Object.assign(live, state.root)
            live.botPresets = state.presets
        },
    })

    await controller.updateActivePresetImage('icon')
    expect(live.botPresets.map(item => item.image)).toEqual([undefined, undefined, 'icon', undefined, undefined])
    await controller.removePreset(0)
    expect(live.botPresets[live.botPresetsId].id).toBe('c')
    expect(chainPick).toBe('b')
    await controller.removePreset(2)
    expect(live.botPresets[live.botPresetsId].id).toBe('c')
    await controller.removePreset(0)
    expect(live.botPresets[live.botPresetsId].id).toBe('c')
    expect(chainPick).toBeNull()
    // Removing the selected preset falls back to the first one.
    await controller.removePreset(0)
    expect(live.botPresets.map(item => item.id)).toEqual(['e'])
    expect(live.botPresetsId).toBe(0)
    expect(live.mainPrompt).toBe('e')
})

it('rejects a confirmed same-name replacement before entering the mutation queue', async () => {
    const live = { botPresetsId: 0, botPresets: [preset('X', 'A'), preset('X', 'B')] } as Database
    const expected = capturePresetNames(live.botPresets)
    live.botPresets[1] = preset('X', 'C')
    const controller = createPresetWorkingSetController({
        getDatabase: () => live,
        captureCurrentPreset: () => null,
        applyPreset: () => {},
        mutatePersistentPresets: async (_reason, mutate) => {
            await mutate({ root: { botPresetsId: 0 } as any, presets: structuredClone(live.botPresets) })
        },
    })
    await expect(controller.removePreset(1, expected)).rejects.toThrow('Preset list changed')
    expect(live.botPresets.map(item => item.mainPrompt)).toEqual(['A', 'C'])
})

it.each(['unchanged', 'replace-during-read'] as const)('reads a preset after its own save publication: %s', async mode => {
    DBState.db = { botPresetsId: 0, botPresets: [preset('X', 'A'), preset('X', 'B')], mainPrompt: 'edited A' } as Database
    const expected = capturePresetNames(DBState.db.botPresets)
    expect(DBState.db.botPresets[1]).toBe(DBState.db.botPresets[1])
    const saved = [preset('X', 'edited A'), preset('X', 'B')]
    runtime.mutatePersistentPresets.mockImplementation(async (_reason, mutate) => {
        const state = { root: { botPresetsId: 0 }, presets: saved }
        await mutate(state)
        DBState.db.botPresets = [...state.presets]
    })
    runtime.store.queryPresets.mockResolvedValue({ revision: 1, items: [
        { id: 'a', configuredIndex: 0, name: 'X' }, { id: 'b', configuredIndex: 1, name: 'X' },
    ] })
    runtime.store.readPreset.mockImplementation(async () => {
        if (mode === 'replace-during-read') DBState.db.botPresets[1] = preset('X', 'C')
        return { revision: 1, value: saved[1] }
    })
    const task = readPresetBodies([1], expected)
    if (mode === 'unchanged') await expect(task).resolves.toEqual([preset('X', 'B')])
    else await expect(task).rejects.toThrow('Preset list changed')
    expect(DBState.db.botPresets[1].mainPrompt).toBe(mode === 'unchanged' ? 'B' : 'C')
})
