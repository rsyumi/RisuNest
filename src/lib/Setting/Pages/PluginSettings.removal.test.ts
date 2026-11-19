// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
const mocks = vi.hoisted(() => ({ confirm: vi.fn(), deleteData: vi.fn(), error: vi.fn(), reload: vi.fn(), mutatePresets: vi.fn() }))
vi.mock('src/ts/alert', () => ({ alertCheckboxConfirm: mocks.confirm, alertError: mocks.error, alertConfirm: vi.fn(), alertMd: vi.fn(), alertSelect: vi.fn(), alertToast: vi.fn() }))
vi.mock('src/ts/plugins/pluginDataInventory', () => ({ deletePluginDataForOwner: mocks.deleteData }))
vi.mock('src/ts/plugins/plugins.svelte', () => ({ checkPluginUpdate: vi.fn(), createBlankPlugin: vi.fn(), importPlugin: vi.fn(), loadPlugins: mocks.reload, updatePlugin: vi.fn() }))
// Removal clears the plugin's provider from the stored presets first.
vi.mock('src/ts/storage/persistentDataRuntime.svelte', async (importOriginal) => ({
    ...await importOriginal<typeof import('src/ts/storage/persistentDataRuntime.svelte')>(),
    mutatePersistentPresets: mocks.mutatePresets,
}))
vi.mock('src/ts/plugins/apiV3/developMode', () => ({ hotReloadPluginFiles: vi.fn() }))
vi.mock('src/ts/plugins/apiV3/v3.svelte', () => ({ resetAllPluginPermissions: vi.fn() }))
vi.mock('src/ts/stores.svelte', async () => ({
    DBState: { db: { plugins: [] as any[], currentPluginProvider: '', pluginPermissions: { synthetic: true } } },
    hotReloading: [], SettingsMenuIndex: (await import('svelte/store')).writable(0),
}))
vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
vi.mock('src/lib/UI/GUI/TextInput.svelte', async () => ({ default: (await import('../../Others/AlertCompDependencyStub.test.svelte')).default }))
vi.mock('src/lib/UI/GUI/NumberInput.svelte', async () => ({ default: (await import('../../Others/AlertCompDependencyStub.test.svelte')).default }))
vi.mock('src/lib/UI/GUI/SelectInput.svelte', async () => ({ default: (await import('../../Others/AlertCompDependencyStub.test.svelte')).default }))
vi.mock('src/lib/UI/GUI/OptionInput.svelte', async () => ({ default: (await import('../../Others/AlertCompDependencyStub.test.svelte')).default }))
vi.mock('src/lib/UI/GUI/CheckInput.svelte', async () => ({ default: (await import('../../Others/AlertCompDependencyStub.test.svelte')).default }))
vi.mock('src/lib/UI/GUI/TextAreaInput.svelte', async () => ({ default: (await import('../../Others/AlertCompDependencyStub.test.svelte')).default }))
import { DBState } from 'src/ts/stores.svelte'
import { language } from 'src/lang'
import PluginSettings from './PluginSettings.svelte'
let mounted: ReturnType<typeof mount> | undefined
let target: HTMLDivElement
const plugin = { name: 'synthetic', displayName: 'Synthetic plugin', version: '3.0', enabled: false, arguments: {} }
beforeEach(() => {
    vi.resetAllMocks()
    DBState.db.plugins = [structuredClone(plugin)] as any
    DBState.db.currentPluginProvider = plugin.name
    target = document.createElement('div'); document.body.append(target)
})
afterEach(async () => { if (mounted) await unmount(mounted); mounted = undefined; document.body.replaceChildren() })
async function remove() {
    mounted = mount(PluginSettings, { target })
    await tick()
    target.querySelector<HTMLButtonElement>('button[aria-label="' + language.risuNest.plugins.removeAction + ' Synthetic plugin"]')!.click()
    await vi.waitFor(() => expect(mocks.confirm).toHaveBeenCalledOnce())
    await tick()
}
describe('plugin removal option', () => {
    it.each([false, true])('removes the plugin with data option %s and retains permissions', async checked => {
        mocks.confirm.mockResolvedValue({ confirmed: true, checked })
        await remove()
        await vi.waitFor(() => expect(DBState.db.plugins).toHaveLength(0))
        expect(mocks.confirm).toHaveBeenCalledWith(expect.objectContaining({ title: language.risuNest.plugins.removeTitle + '\nSynthetic plugin', requireChecked: false }))
        expect(mocks.deleteData).toHaveBeenCalledTimes(checked ? 1 : 0)
        if (checked) expect(mocks.deleteData).toHaveBeenCalledWith('synthetic')
        expect(DBState.db.currentPluginProvider).toBe('')
        expect((DBState.db as any).pluginPermissions).toEqual({ synthetic: true })
    })
    it('removes by owner name when the record is replaced during confirmation', async () => {
        let confirm!: (result: { confirmed: boolean; checked: boolean }) => void
        mocks.confirm.mockImplementation(() => new Promise(resolve => { confirm = resolve }))
        await remove()
        DBState.db.plugins = [{ ...plugin, displayName: 'Updated plugin' }] as any
        confirm({ confirmed: true, checked: true })
        await vi.waitFor(() => expect(DBState.db.plugins).toHaveLength(0))
        expect(mocks.deleteData).toHaveBeenCalledWith('synthetic')
        expect(DBState.db.currentPluginProvider).toBe('')
    })
    it('deletes the data only after the reload has stopped the plugin', async () => {
        let finishReload!: () => void
        mocks.confirm.mockResolvedValue({ confirmed: true, checked: true })
        mocks.reload.mockImplementation(() => new Promise<void>(resolve => { finishReload = resolve }))
        await remove()
        await vi.waitFor(() => expect(mocks.reload).toHaveBeenCalledOnce())
        expect(DBState.db.plugins).toHaveLength(0)
        expect(DBState.db.currentPluginProvider).toBe('')
        await new Promise(resolve => setTimeout(resolve, 0))
        expect(mocks.deleteData).not.toHaveBeenCalled()
        finishReload()
        await vi.waitFor(() => expect(mocks.deleteData).toHaveBeenCalledExactlyOnceWith('synthetic'))
    })
    it('removes the plugin, keeps its permissions and shows a deletion failure', async () => {
        mocks.confirm.mockResolvedValue({ confirmed: true, checked: true })
        mocks.deleteData.mockRejectedValue(new Error('deletion failed'))
        await remove()
        await vi.waitFor(() => expect(mocks.error).toHaveBeenCalled())
        expect(DBState.db.plugins).toHaveLength(0)
        expect(DBState.db.currentPluginProvider).toBe('')
        expect((DBState.db as any).pluginPermissions).toEqual({ synthetic: true })
        expect(mocks.reload).toHaveBeenCalledOnce()
    })
    it('keeps the plugin and the provider it shows when the stored presets cannot be cleared', async () => {
        mocks.confirm.mockResolvedValue({ confirmed: true, checked: true })
        mocks.mutatePresets.mockRejectedValue(new Error('clear failed'))
        await remove()
        await vi.waitFor(() => expect(mocks.error).toHaveBeenCalled())
        expect(DBState.db.plugins).toHaveLength(1)
        expect(DBState.db.currentPluginProvider).toBe('synthetic')
        expect(mocks.deleteData).not.toHaveBeenCalled()
    })
    it('keeps the data when the reload fails', async () => {
        mocks.confirm.mockResolvedValue({ confirmed: true, checked: true })
        mocks.reload.mockRejectedValue(new Error('unload failed'))
        await remove()
        await vi.waitFor(() => expect(mocks.error).toHaveBeenCalled())
        expect(mocks.deleteData).not.toHaveBeenCalled()
    })
    it('cancels without removing the plugin or its data', async () => {
        mocks.confirm.mockResolvedValue({ confirmed: false, checked: true })
        await remove()
        expect(DBState.db.plugins).toHaveLength(1)
        expect(mocks.deleteData).not.toHaveBeenCalled()
    })
})
