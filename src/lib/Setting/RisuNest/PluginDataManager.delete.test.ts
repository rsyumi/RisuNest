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
vi.mock('src/ts/storage/localDataSections', () => ({
    readLocalDataParticipation: async () => [],
}))
const alerts = vi.hoisted(() => ({ alertConfirm: vi.fn(), alertSelect: vi.fn() }))
vi.mock('src/ts/alert', () => alerts)
const inventory = vi.hoisted(() => ({
    listPluginDataItems: vi.fn(),
    deletePluginDataItems: vi.fn(async () => {}),
}))
vi.mock('src/ts/plugins/pluginDataInventory', async (importOriginal) => ({
    ...(await importOriginal<typeof import('src/ts/plugins/pluginDataInventory')>()),
    listPluginDataItems: inventory.listPluginDataItems,
    deletePluginDataItems: inventory.deletePluginDataItems,
}))

import PluginDataManager from './PluginDataManager.svelte'
import { languageEnglish } from 'src/lang/en'
import type { PluginDataItem } from 'src/ts/plugins/pluginDataInventory'

const strings = languageEnglish.risuNest.pluginData
const items: PluginDataItem[] = ['pm_store', 'pm_keys', 'yt_glossary'].map((key) => ({
    owner: 'provider-manager',
    key,
    valueType: 'json',
    byteSize: 24,
    automatic: false,
}))

let component: ReturnType<typeof mount> | undefined
let target: HTMLDivElement | undefined

async function settle(): Promise<void> {
    for (let index = 0; index < 8; index += 1) await tick()
}

async function open(): Promise<HTMLDivElement> {
    inventory.listPluginDataItems.mockResolvedValue(items)
    target = document.createElement('div')
    document.body.append(target)
    component = mount(PluginDataManager, { target, props: { place: 'settings' } })
    await settle()
    return target
}

function button(root: HTMLElement, text: string): HTMLButtonElement | undefined {
    return [...root.querySelectorAll<HTMLButtonElement>('button')].find(
        (candidate) => candidate.textContent?.trim() === text,
    )
}

afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    target?.remove()
    target = undefined
    vi.clearAllMocks()
})

describe('plugin data manager deletions', () => {
    it('keeps the list inside its own scroll area', async () => {
        const root = await open()
        const list = root.querySelector('[data-plugin-data-list]')
        expect(list?.className).toContain('overflow-y-auto')
        expect(list?.className).toMatch(/max-h-/)
        expect(root.querySelectorAll('[data-plugin-data-row]')).toHaveLength(3)
    })

    it('asks before deleting one value and does nothing when declined', async () => {
        const root = await open()
        alerts.alertConfirm.mockResolvedValue(false)
        root.querySelector<HTMLButtonElement>('[data-plugin-data-row="pm_store"] button[aria-label]')?.click()
        await vi.waitFor(() =>
            expect(alerts.alertConfirm).toHaveBeenCalledWith(strings.deleteConfirmOne),
        )
        await settle()
        expect(inventory.deletePluginDataItems).not.toHaveBeenCalled()

        alerts.alertConfirm.mockResolvedValue(true)
        root.querySelector<HTMLButtonElement>('[data-plugin-data-row="pm_store"] button[aria-label]')?.click()
        await vi.waitFor(() =>
            expect(inventory.deletePluginDataItems).toHaveBeenCalledWith([items[0]]),
        )
    })

    it('asks twice before deleting everything and stops after the first no', async () => {
        const root = await open()
        alerts.alertConfirm.mockResolvedValueOnce(true).mockResolvedValueOnce(false)
        button(root, strings.deleteAll.replace('{0}', '3'))?.click()
        await vi.waitFor(() =>
            expect(alerts.alertConfirm.mock.calls.map(([message]) => message)).toEqual([
                strings.deleteAllConfirm.replace('{0}', '3'),
                strings.deleteAllConfirmFinal,
            ]),
        )
        await settle()
        expect(inventory.deletePluginDataItems).not.toHaveBeenCalled()

        alerts.alertConfirm.mockReset()
        alerts.alertConfirm.mockResolvedValue(true)
        button(root, strings.deleteAll.replace('{0}', '3'))?.click()
        await vi.waitFor(() =>
            expect(inventory.deletePluginDataItems).toHaveBeenCalledWith(items),
        )
        expect(alerts.alertConfirm).toHaveBeenCalledTimes(2)
    })

    it('asks twice with the shown-values wording when a filter narrows the list', async () => {
        const root = await open()
        const search = root.querySelector<HTMLInputElement>('input[type="search"]')!
        search.value = 'pm_'
        search.dispatchEvent(new Event('input', { bubbles: true }))
        await settle()
        expect(root.querySelectorAll('[data-plugin-data-row]')).toHaveLength(2)
        alerts.alertConfirm.mockResolvedValue(true)
        button(root, strings.deleteVisible.replace('{0}', '2'))?.click()
        await vi.waitFor(() =>
            expect(inventory.deletePluginDataItems).toHaveBeenCalledWith(items.slice(0, 2)),
        )
        expect(alerts.alertConfirm.mock.calls.map(([message]) => message)).toEqual([
            strings.deleteVisibleConfirm.replace('{0}', '2'),
            strings.deleteVisibleConfirmFinal,
        ])
    })
})
