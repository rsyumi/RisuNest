import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { Database, botPreset } from './database.svelte'
import {
    createPresetWorkingSetController,
    readPersistentPresetBody,
    readPersistentPresetBodies,
} from './presetWorkingSetOperations'

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

    it('copies an inactive full body instead of its catalog stub', async () => {
        await controller.copyPreset(1)

        expect(persisted[2]).toEqual(preset('Second Copy', 'stored second'))
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
