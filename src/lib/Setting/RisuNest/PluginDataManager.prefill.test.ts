// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

vi.mock('src/lang', async () => ({
    language: (await import('src/lang/en')).languageEnglish,
}))
vi.mock('src/ts/platform', () => ({ isTauri: false }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
vi.mock('src/ts/stores.svelte', () => ({ DBState: { db: { plugins: [] } } }))
vi.mock('src/ts/plugins/plugins.svelte', () => ({
    pluginStorageStore: { invalidate: vi.fn() },
}))
vi.mock('src/ts/storage/persistentDataStoreFactory', () => ({
    getPersistentDataStore: () => ({ listPluginStorage: async () => [] }),
}))
vi.mock('src/ts/storage/localDataSections', () => ({
    readLocalDataParticipation: async () => [],
}))

import PluginDataManager from './PluginDataManager.svelte'
import { UNOWNED_PLUGIN_OWNER } from 'src/ts/plugins/pluginOwner'
import type { PluginDataItem } from 'src/ts/plugins/pluginDataInventory'

const staged: PluginDataItem[] = ['pm_store', 'pm_keys', 'yt_glossary'].map(
    (key) => ({
        owner: UNOWNED_PLUGIN_OWNER,
        key,
        valueType: 'json',
        byteSize: 24,
        automatic: false,
    }),
)

let component: ReturnType<typeof mount> | undefined
let target: HTMLDivElement | undefined

async function openImportStage(
    initialAssignments: { owner: string; keys: string[] }[],
    pluginNames = ['provider-manager', 'yumi-translator'],
): Promise<{ owner: string; keys: string[] }[][]> {
    const published: { owner: string; keys: string[] }[][] = []
    target = document.createElement('div')
    document.body.append(target)
    component = mount(PluginDataManager, {
        target,
        props: {
            place: 'import',
            staged,
            pluginNames,
            initialAssignments,
            onselectionchange: (assignments) => {
                published.push(
                    assignments.map((assignment) => ({
                        owner: assignment.owner,
                        keys: assignment.items.map((item) => item.key),
                    })),
                )
            },
        },
    })
    await tick()
    return published
}

afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    target?.remove()
    target = undefined
})

describe('plugin data manager import stage', () => {
    it('opens on the answers a cancelled import kept', async () => {
        const published = await openImportStage([
            { owner: 'provider-manager', keys: ['pm_store', 'pm_keys'] },
            { owner: 'yumi-translator', keys: ['yt_glossary'] },
        ])

        expect(published.at(-1)).toEqual([
            { owner: 'provider-manager', keys: ['pm_store', 'pm_keys'] },
            { owner: 'yumi-translator', keys: ['yt_glossary'] },
        ])
        const chosen = target?.querySelector<HTMLSelectElement>(
            '[data-plugin-data-group="pm_"] select',
        )
        expect(chosen?.value).toBe('provider-manager')
    })

    it('leaves the answers out when the save no longer carries the plugin', async () => {
        const published = await openImportStage(
            [{ owner: 'uninstalled-plugin', keys: ['pm_store', 'pm_keys'] }],
            ['provider-manager'],
        )

        expect(published.at(-1)).toEqual([])
        const chosen = target?.querySelector<HTMLSelectElement>(
            '[data-plugin-data-group="pm_"] select',
        )
        expect(chosen?.value).toBe('')
    })

    it('opens on nothing when no answers were kept', async () => {
        const published = await openImportStage([])

        expect(published).toEqual([])
        const chosen = target?.querySelector<HTMLSelectElement>(
            '[data-plugin-data-group="pm_"] select',
        )
        expect(chosen?.value).toBe('')
    })
})
