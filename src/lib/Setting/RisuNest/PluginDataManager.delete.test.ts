// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, settled as domSettled, unmount } from 'svelte'

vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'tauri', os: 'windows' }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: vi.fn() }))
vi.mock('src/ts/stores.svelte', () => ({ DBState: { db: { plugins: [] } } }))
vi.mock('src/ts/plugins/plugins.svelte', () => ({
    pluginStorageStore: { invalidate: vi.fn() },
}))
vi.mock('src/ts/storage/localDataSections', () => ({
    readLocalDataParticipation: async () => [],
}))
const alerts = vi.hoisted(() => ({ alertConfirm: vi.fn(), alertCheckboxConfirm: vi.fn(), alertSelect: vi.fn() }))
vi.mock('src/ts/alert', () => alerts)
const inventory = vi.hoisted(() => ({
    listPluginDataItems: vi.fn(),
    deletePluginDataItems: vi.fn(async () => {}),
    readPluginDataValue: vi.fn(async () => 'matching value'),
    searchPluginDataValues: vi.fn(async (items: { owner: string; key: string; space?: string }[]) => new Set(
        items.map((item) => JSON.stringify([item.space ?? '', item.owner, item.key])))),
}))
vi.mock('src/ts/plugins/pluginDataInventory', async (importOriginal) => ({
    ...(await importOriginal<typeof import('src/ts/plugins/pluginDataInventory')>()),
    listPluginDataItems: inventory.listPluginDataItems,
    deletePluginDataItems: inventory.deletePluginDataItems,
    readPluginDataValue: inventory.readPluginDataValue,
    searchPluginDataValues: inventory.searchPluginDataValues,
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
    for (let index = 0; index < 8; index += 1) await domSettled()
}

async function open(seed = true, pluginNames?: string[]): Promise<HTMLDivElement> {
    if (seed) inventory.listPluginDataItems.mockResolvedValue(items)
    target = document.createElement('div')
    document.body.append(target)
    component = mount(PluginDataManager, { target, props: { place: 'settings', pluginNames } })
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
    inventory.listPluginDataItems.mockReset()
    inventory.readPluginDataValue.mockReset().mockResolvedValue('matching value')
    inventory.searchPluginDataValues.mockReset().mockImplementation(async (items) => new Set(
        items.map((item: PluginDataItem) => JSON.stringify([item.space ?? '', item.owner, item.key]))))
    vi.useRealTimers()
})

describe('plugin data manager deletions', () => {
    it.each([true, false])('rejects a stale scope listing (old first: %s)', async (oldFirst) => {
        let old!: (rows: PluginDataItem[]) => void
        let current!: (rows: PluginDataItem[]) => void
        inventory.listPluginDataItems
            .mockImplementationOnce(() => new Promise(resolve => { old = resolve }))
            .mockImplementationOnce(() => new Promise(resolve => { current = resolve }))
        const root = await open(false)
        button(root, strings.scopeThisDevice)!.click()
        await settle()
        const device = [{ ...items[0], key: 'device_value', space: 'json' as const }]
        if (oldFirst) { old(items); await settle(); current(device) }
        else { current(device); await settle(); old(items) }
        await settle()
        expect(root.querySelector('[data-plugin-data-row="pm_store"]')).toBeNull()
        expect(root.querySelector('[data-plugin-data-row="device_value"]')).not.toBeNull()
    })

    it('cancels deletion when the scope changes during confirmation', async () => {
        const root = await open()
        let confirm!: (answer: boolean) => void
        alerts.alertConfirm.mockImplementationOnce(() => new Promise(resolve => { confirm = resolve }))
        root.querySelector<HTMLButtonElement>('[data-plugin-data-row="pm_store"] button[aria-label]')!.click()
        await vi.waitFor(() => expect(alerts.alertConfirm).toHaveBeenCalled())
        inventory.listPluginDataItems.mockResolvedValue([])
        button(root, strings.scopeThisDevice)!.click()
        await settle()
        confirm(true)
        await settle()
        expect(inventory.deletePluginDataItems).not.toHaveBeenCalled()
    })

    it('searches while typing and repeats the retained query after refresh', async () => {
        const root = await open()
        vi.useFakeTimers()
        const search = root.querySelector<HTMLInputElement>(`input[placeholder="${strings.searchValue}"]`)!
        search.value = 'matching'
        search.dispatchEvent(new Event('input', { bubbles: true }))
        await settle()
        expect(root.querySelector('[role="status"]')?.textContent).toBe(languageEnglish.loading)
        expect(root.textContent).not.toContain(strings.empty)
        await vi.advanceTimersByTimeAsync(250)
        await settle()
        expect(root.querySelectorAll('[data-plugin-data-row]')).toHaveLength(3)
        button(root, strings.refresh)!.click()
        await settle()
        expect(root.querySelector('[role="status"]')).not.toBeNull()
        await vi.advanceTimersByTimeAsync(250)
        await settle()
        expect(inventory.searchPluginDataValues).toHaveBeenCalledTimes(2)
        expect(root.querySelectorAll('[data-plugin-data-row]')).toHaveLength(3)
    })

    it('does not publish a value search completed after a scope switch', async () => {
        const root = await open()
        vi.useFakeTimers()
        let finish!: (value: Set<string>) => void
        inventory.searchPluginDataValues.mockImplementationOnce(() => new Promise(resolve => { finish = resolve }))
        const search = root.querySelector<HTMLInputElement>(`input[placeholder="${strings.searchValue}"]`)!
        search.value = 'matching'
        search.dispatchEvent(new Event('input', { bubbles: true }))
        await settle()
        await vi.advanceTimersByTimeAsync(250)
        inventory.listPluginDataItems.mockResolvedValue([{ ...items[0], key: 'device_value', space: 'json' }])
        button(root, strings.scopeThisDevice)!.click()
        await settle()
        await vi.advanceTimersByTimeAsync(250)
        finish(new Set(items.map((item) => JSON.stringify(['', item.owner, item.key]))))
        await settle()
        expect(inventory.searchPluginDataValues).toHaveBeenCalledTimes(2)
        expect(root.querySelectorAll('[data-plugin-data-row]')).toHaveLength(1)
        expect(root.querySelector('[data-plugin-data-row="device_value"]')).not.toBeNull()
    })

    it('reloads a retained value query when the key filter is widened', async () => {
        const root = await open()
        vi.useFakeTimers()
        const key = root.querySelector<HTMLInputElement>(`input[placeholder="${strings.searchKey}"]`)!
        const value = root.querySelector<HTMLInputElement>(`input[placeholder="${strings.searchValue}"]`)!
        key.value = 'pm_'
        key.dispatchEvent(new Event('input', { bubbles: true }))
        value.value = 'matching'
        value.dispatchEvent(new Event('input', { bubbles: true }))
        await settle()
        await vi.advanceTimersByTimeAsync(250)
        await settle()
        expect(root.querySelectorAll('[data-plugin-data-row]')).toHaveLength(2)
        key.value = ''
        key.dispatchEvent(new Event('input', { bubbles: true }))
        await settle()
        expect(root.querySelector('[role="status"]')).not.toBeNull()
        await vi.advanceTimersByTimeAsync(250)
        await settle()
        expect(root.querySelectorAll('[data-plugin-data-row]')).toHaveLength(3)
        expect(inventory.searchPluginDataValues).toHaveBeenCalledTimes(2)
    })

    it('offers reload after deleting data for an installed owner', async () => {
        const root = await open(true, ['provider-manager'])
        alerts.alertConfirm.mockResolvedValue(true)
        root.querySelector<HTMLButtonElement>('[data-plugin-data-row="pm_store"] button[aria-label]')!.click()
        await vi.waitFor(() => expect(root.textContent).toContain(strings.assign.reloadTitle))
    })

    it('keeps the list inside its own scroll area', async () => {
        const root = await open()
        const list = root.querySelector('[data-plugin-data-list]')
        expect(list?.className).toContain('overflow-y-auto')
        expect(list?.className).toMatch(/max-h-/)
        expect(root.querySelectorAll('[data-plugin-data-row]')).toHaveLength(3)
    })

    it('names both search fields for screen readers', async () => {
        const root = await open()
        const names = [...root.querySelectorAll<HTMLInputElement>('input[type="text"]')].map(
            (input) => root.querySelector(`label[for="${input.id}"]`)?.textContent,
        )
        expect(names).toEqual([strings.searchKey, strings.searchValue])
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
            expect(inventory.deletePluginDataItems).toHaveBeenCalledWith([items[0]], 'library'),
        )
    })

    it('asks once with acknowledgement before deleting orphaned plugin data', async () => {
        const root = await open(true, [])
        expect(root.querySelector('[data-plugin-data-row="pm_store"]')).not.toBeNull()
        alerts.alertCheckboxConfirm.mockResolvedValueOnce({ confirmed: false, checked: false })
        button(root, strings.deleteAll.replace('{0}', '3'))!.click()
        await vi.waitFor(() => expect(alerts.alertCheckboxConfirm).toHaveBeenCalledOnce())
        expect(alerts.alertCheckboxConfirm).toHaveBeenCalledWith(expect.objectContaining({
            title: strings.deleteAllConfirm.replace('{0}', '3'),
            description: strings.deleteAllConfirmFinal,
            requireChecked: true,
        }))
        expect(inventory.deletePluginDataItems).not.toHaveBeenCalled()
        alerts.alertCheckboxConfirm.mockResolvedValue({ confirmed: true, checked: true })
        button(root, strings.deleteAll.replace('{0}', '3'))!.click()
        await vi.waitFor(() => expect(inventory.deletePluginDataItems).toHaveBeenCalledWith(items, 'library'))
        expect(alerts.alertConfirm).not.toHaveBeenCalled()
    })

    it('acknowledges only the shown values when a filter narrows the list', async () => {
        const root = await open()
        const search = [...root.querySelectorAll<HTMLInputElement>('input')].find(input => input.placeholder === strings.searchKey)!
        search.value = 'pm_'
        search.dispatchEvent(new Event('input', { bubbles: true }))
        await settle()
        alerts.alertCheckboxConfirm.mockResolvedValue({ confirmed: true, checked: true })
        button(root, strings.deleteVisible.replace('{0}', '2'))!.click()
        await vi.waitFor(() => expect(inventory.deletePluginDataItems).toHaveBeenCalledWith(items.slice(0, 2), 'library'))
        expect(alerts.alertCheckboxConfirm).toHaveBeenCalledWith(expect.objectContaining({
            title: strings.deleteVisibleConfirm.replace('{0}', '2'), description: strings.deleteVisibleConfirmFinal, requireChecked: true,
        }))
        expect(alerts.alertConfirm).not.toHaveBeenCalled()
    })

})
