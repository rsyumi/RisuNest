import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { RisuPlugin } from '../plugins.svelte'

const fixture = vi.hoisted(() => ({
    database: { characters: [], plugins: [], currentPluginProvider: '' } as any,
    // The library plugin storage both the sandbox and the data deletion use.
    storage: new Map<string, Map<string, unknown>>(),
    // Provider names the preset store is asked to clear.
    clearProviders: vi.fn(async (_names: string[]) => undefined),
}))

function ownedStorage(owner: string) {
    const values = () => {
        let owned = fixture.storage.get(owner)
        if (!owned) fixture.storage.set(owner, owned = new Map())
        return owned
    }
    return {
        dispose: () => {},
        getItem: async (key: string) => values().get(key) ?? null,
        setItem: async (key: string, value: unknown) => { values().set(key, value) },
        removeItem: async (key: string) => { values().delete(key) },
        clear: async () => values().clear(),
        key: async (index: number) => [...values().keys()][index] ?? null,
        keys: async () => [...values().keys()],
        length: async () => values().size,
        snapshot: async () => Object.fromEntries(values()),
        mutate: async (mutations: { type: 'set' | 'delete'; key: string; value?: unknown }[]) => {
            for (const mutation of mutations) {
                if (mutation.type === 'delete') values().delete(mutation.key)
                else values().set(mutation.key, mutation.value)
            }
        },
    }
}

vi.mock('../plugins.svelte', async () => {
    const host = await import('./v3Host.testSupport')
    const { createPluginLoadOrchestrator } = await import('../pluginCompatibility')
    const load = createPluginLoadOrchestrator<RisuPlugin>({
        resetRegistry: async () => undefined,
        loadV3: async (plugins) => (await import('./v3.svelte')).loadV3Plugins([...plugins]),
    })
    return host.pluginsModule({
        loadPlugins: () => load(fixture.database.plugins.filter((plugin: RisuPlugin) => plugin.enabled)),
        pluginStorageStore: host.pluginStorageStoreStub('removal-plugin', ownedStorage),
    })
})
vi.mock('../../storage/persistentDataStoreFactory', () => ({
    getPersistentDataStore: () => ({
        listPluginStorage: async () => [...fixture.storage].flatMap(([owner, values]) => [...values].map(([key, value]) => ({
            owner, key, valueType: typeof value === 'string' ? 'string' : 'json', byteSize: JSON.stringify(value).length,
        }))),
    }),
}))
vi.mock('src/ts/storage/database.svelte', async () => (await import('./v3Host.testSupport')).databaseModule({ read: () => fixture.database }, {
    clearPresetPluginProvider: fixture.clearProviders,
}))
vi.mock('../pluginSafeClass', async () => (await import('./v3Host.testSupport')).pluginSafeClassModule())
vi.mock('src/ts/stores.svelte', async () => (await import('./v3Host.testSupport')).storesModule({ read: () => fixture.database }))
vi.mock('src/ts/alert', async () => (await import('./v3Host.testSupport')).alertModule())
// Unload callbacks race this timeout, so it has to be a real one.
vi.mock('src/ts/util', async () => (await import('./v3Host.testSupport')).utilModule({ sleep: (ms: number) => new Promise((resolve) => setTimeout(resolve, ms)) }))
vi.mock('src/ts/globalApi.svelte', () => ({
    checkCharOrder: vi.fn(), forageStorage: {}, getFetchLogs: vi.fn(),
}))
vi.mock('src/ts/gui/colorscheme', async () => (await import('./v3Host.testSupport')).colorschemeModule())
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'web', os: 'windows' }))
vi.mock('src/ts/process/mcp/pluginmcp', async () => (await import('./v3Host.testSupport')).pluginMcpModule())
vi.mock('src/ts/process/files/inlays', async () => (await import('./v3Host.testSupport')).inlaysModule())
vi.mock('src/ts/translator/translator', async () => (await import('./v3Host.testSupport')).translatorModule())
vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
vi.mock('localforage', async () => (await import('../localforage.testSupport')).localforageModule())
vi.mock('src/ts/process/index.svelte', async () => (await import('./v3Host.testSupport')).processIndexModule())
vi.mock('src/ts/model/modellist', async () => (await import('./v3Host.testSupport')).modellistModule())
vi.mock('src/ts/process/request/request', async () => (await import('./v3Host.testSupport')).requestModule())
vi.mock('src/ts/process/modules', async () => (await import('./v3Host.testSupport')).modulesModule())
vi.mock('src/ts/process/ttsHooks', async () => (await import('./v3Host.testSupport')).ttsHooksModule())
vi.mock('src/ts/storage/persistentDataRuntime.svelte', async () => (await import('./v3Host.testSupport')).persistentRuntimeModule())

import { listPluginDataItems } from '../pluginDataInventory'
import { removeInstalledPlugin } from '../pluginRemoval'
import { loadPlugins } from '../plugins.svelte'
import { loadV3Plugins } from './v3.svelte'

const owner = 'removal-plugin'
const removable: RisuPlugin = {
    name: owner,
    script: `
window.ready = (async () => {
    await risuai.onUnload(async () => {
        await risuai.pluginStorage.setItem('saved-while-unloading', { cache: 'flushed' })
    })
    await risuai.addProvider('custom-provider', async () => ({ success: true, content: '' }))
    await risuai.pluginStorage.setItem('saved-while-running', 'value')
})()
`,
    arguments: {},
    realArg: {},
    version: '3.0',
    customLink: [],
    argMeta: {},
    enabled: true,
}

async function executeIframeSrcdoc(frame: HTMLIFrameElement): Promise<void> {
    const child = frame.contentWindow!
    const childRealm = child as any
    Object.defineProperty(child, 'ImageBitmap', {
        configurable: true,
        value: globalThis.ImageBitmap,
    })
    vi.spyOn(window, 'postMessage').mockImplementation((data) => {
        window.dispatchEvent(new MessageEvent('message', { data, source: child }))
    })
    vi.spyOn(child.parent, 'postMessage').mockImplementation((data) => {
        window.dispatchEvent(new MessageEvent('message', { data, source: child }))
    })
    vi.spyOn(child, 'postMessage').mockImplementation((data, _origin, transfer?: Transferable[]) => {
        child.dispatchEvent(new childRealm.MessageEvent('message', { data, source: child.parent, ports: transfer ?? [] }))
    })
    const source = frame.srcdoc.match(/<script nonce="[^"]+">([\s\S]*)<\/script>/)?.[1]
    if (!source) throw new Error('Sandbox guest script was not found')
    await childRealm.eval(source)
}

async function runInstalledPlugin(): Promise<void> {
    fixture.database.plugins = [structuredClone(removable)]
    fixture.database.currentPluginProvider = 'custom-provider'
    await loadPlugins()
    const frame = document.querySelector<HTMLIFrameElement>('iframe[data-risu-plugin-frame]')
    if (!frame) throw new Error('Plugin iframe was not created')
    await executeIframeSrcdoc(frame)
    await (frame.contentWindow as any).ready
}

async function ownedKeys(): Promise<string[]> {
    return (await listPluginDataItems('library')).filter((item) => item.owner === owner).map((item) => item.key).sort()
}

beforeEach(() => {
    vi.clearAllMocks()
    vi.stubGlobal('ImageBitmap', class {})
    fixture.storage.clear()
    vi.spyOn(console, 'log').mockImplementation(() => undefined)
})

afterEach(async () => {
    await loadV3Plugins([])
    vi.restoreAllMocks()
})

describe('plugin removal with a running sandbox', () => {
    it('clears the providers it registered and leaves no data behind that the plugin saved while unloading', async () => {
        await runInstalledPlugin()
        expect(await ownedKeys()).toEqual(['saved-while-running'])
        // A removal that fails to clear the stored presets keeps the plugin for another try.
        fixture.clearProviders.mockRejectedValueOnce(new Error('Preset store failed'))
        await expect(removeInstalledPlugin(owner, true)).rejects.toThrow('Preset store failed')
        expect(fixture.database.plugins.map((plugin: RisuPlugin) => plugin.name)).toEqual([owner])

        await removeInstalledPlugin(owner, true)

        expect(fixture.database.plugins).toEqual([])
        expect(fixture.database.currentPluginProvider).toBe('')
        // Presets that are not loaded are cleared in the store.
        expect(fixture.clearProviders).toHaveBeenLastCalledWith(['custom-provider', owner])
        expect(document.querySelector('iframe[data-risu-plugin-frame]')).toBeNull()
        expect(await ownedKeys()).toEqual([])
    })

    it('keeps what the plugin saved while unloading when the data is kept', async () => {
        await runInstalledPlugin()

        await removeInstalledPlugin(owner, false)

        expect(fixture.database.plugins).toEqual([])
        expect(await ownedKeys()).toEqual(['saved-while-running', 'saved-while-unloading'])
    })
})
