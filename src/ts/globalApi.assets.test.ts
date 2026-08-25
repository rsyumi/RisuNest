import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'
import { setRuntimePerformanceProfile } from './runtimePerformanceProfile'

const state = vi.hoisted(() => ({
    isTauri: false,
    blobStore: null as any,
}))

vi.mock('./platform', () => ({
    get isTauri() { return state.isTauri },
    get isTauriMobile() { return false },
    isNodeServer: false,
}))
vi.mock('./storage/platformBlobStore', () => ({
    configureBlobStoreStorageProvider: vi.fn(),
    readBlobForFacade: vi.fn(),
    resolveBlobStore: async () => state.blobStore,
}))
vi.mock('./storage/autoStorage', () => ({
    AutoStorage: class {
        isAccount = false
        realStorage = {}
        async Init() { /* no-op */ }
        async getItem() { return null }
        async setItem() { /* no-op */ }
        async removeItem() { /* no-op */ }
        async keys() { return [] }
    },
}))
vi.mock('./characterCards', () => ({
    hubURL: 'https://hub.example',
    characterURLImport: vi.fn(),
}))
vi.mock('./util', () => ({
    changeFullscreen: vi.fn(),
    sleep: async () => undefined,
}))
vi.mock('./storage/database.svelte', () => ({
    getDatabase: () => ({}),
    getCurrentCharacter: () => ({ chats: [], chatPage: 0 }),
    defaultSdDataFunc: () => [],
    appVer: '0.0.0',
    appSubVer: '',
}))
vi.mock('./stores.svelte', () => ({
    MobileGUI: { set: vi.fn() },
    botMakerMode: { set: vi.fn() },
    selectedCharID: { set: vi.fn() },
    loadedStore: { set: vi.fn() },
    DBState: { db: { characters: [] } },
    LoadingStatusState: {},
    selIdState: { selId: 0 },
    ReloadGUIPointer: { set: vi.fn() },
    bodyIntercepterStore: [],
}))
vi.mock('./alert', () => ({
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertMd: vi.fn(),
    alertNormal: vi.fn(),
    alertNormalWait: vi.fn(),
    alertSelect: vi.fn(),
    alertTOS: vi.fn(),
    waitAlert: vi.fn(),
}))
vi.mock('./storage/persistentDataRuntime.svelte', () => ({
    activateConversation: vi.fn(),
    configurePersistentDataRuntime: vi.fn(),
    markPersistentDataDirty: vi.fn(),
    replacePersistentDatabase: vi.fn(),
}))
vi.mock('./storage/persistentSaveNotifications', () => ({
    createPersistentSaveObserverInstallation: () => ({ install: vi.fn() }),
    installPersistentSaveNotifications: vi.fn(),
}))
vi.mock('./storage/nodeStorage', () => ({ getNodeServerProxyAuth: vi.fn() }))
vi.mock('./storage/databasePreparation', () => ({ checkCharOrder: vi.fn() }))
vi.mock('./storage/risuSave', () => ({ decodeRisuSave: vi.fn() }))
vi.mock('./storage/defaultPrompts', () => ({
    defaultJailbreak: '', defaultMainPrompt: '', oldJailbreak: '', oldMainPrompt: '',
}))
vi.mock('./process/modules', () => ({ moduleUpdate: vi.fn() }))
vi.mock('./process/coldstorage.svelte', () => ({
    getColdStorageItem: vi.fn(),
    makeColdData: vi.fn(),
}))
vi.mock('./process/coldstorageData', () => ({
    listCharacterResources: () => [],
    listDatabaseRootResources: () => [],
    replaceCharacterResources: vi.fn(),
    replaceDatabaseRootResources: vi.fn(),
}))
vi.mock('./plugins/plugins.svelte', () => ({ loadPlugins: vi.fn() }))
vi.mock('./parser/parser.svelte', () => ({ hasher: vi.fn(async () => 'hashed') }))
vi.mock('./drive/drive', () => ({ checkDriverInit: vi.fn() }))
vi.mock('./drive/accounter', () => ({ loadRisuAccountData: vi.fn() }))
vi.mock('./update', () => ({ checkRisuUpdate: vi.fn() }))
vi.mock('./observer.svelte', () => ({ startObserveDom: vi.fn() }))
vi.mock('./characters', () => ({ updateLorebooks: vi.fn() }))
vi.mock('./hotkey', () => ({ initMobileGesture: vi.fn() }))
vi.mock('./gui/animation', () => ({ updateAnimationSpeed: vi.fn() }))
vi.mock('./gui/colorscheme', () => ({ updateColorScheme: vi.fn(), updateTextThemeAndCSS: vi.fn() }))
vi.mock('./gui/guisize', () => ({ updateGuisize: vi.fn() }))
vi.mock('src/lang', () => ({ language: {} }))
vi.mock('streamsaver', () => ({ default: { createWriteStream: vi.fn() } }))
vi.mock('@tauri-apps/plugin-fs', () => ({
    writeFile: vi.fn(), readFile: vi.fn(), exists: vi.fn(), mkdir: vi.fn(),
    readDir: vi.fn(), remove: vi.fn(), BaseDirectory: { Download: 1, AppData: 2 },
}))
vi.mock('@tauri-apps/api/core', () => ({ convertFileSrc: (path: string) => `asset://${path}` }))
vi.mock('@tauri-apps/api/path', () => ({ appDataDir: vi.fn(async () => '/data'), join: vi.fn(async (...parts: string[]) => parts.join('/')) }))
vi.mock('@tauri-apps/plugin-shell', () => ({ open: vi.fn() }))
vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn() }))
vi.mock('@tauri-apps/api/webviewWindow', () => ({ getCurrentWebviewWindow: vi.fn(() => ({})) }))
vi.mock('@tauri-apps/plugin-http', () => ({ fetch: vi.fn() }))

import { createThrottledSizeEstimator, forageStorage, getFileSrc, saveAsset } from './globalApi.svelte'

function createFakeBlobStore(entries: { [key: string]: { data: Uint8Array, mime: string } }) {
    return {
        read: vi.fn(async (key: string) => entries[key]?.data ?? null),
        stat: vi.fn(async (key: string) => entries[key] ? { mime: entries[key].mime } : null),
        resolveUrl: vi.fn(async (key: string) => entries[key] ? `asset:///data/${key}` : null),
        put: vi.fn(async (key: string, data: Uint8Array, metadata: { mime: string, ext: string }) => {
            entries[key] = { data, mime: metadata.mime || 'application/octet-stream' }
            return metadata
        }),
        list: vi.fn(async () => []),
        remove: vi.fn(async () => undefined),
    }
}

beforeEach(() => {
    setRuntimePerformanceProfile('normal')
    state.isTauri = false
    ;(forageStorage as any).isAccount = false
    vi.spyOn(console, 'error').mockImplementation(() => undefined)
})

afterEach(() => {
    vi.useRealTimers()
    vi.restoreAllMocks()
})

describe('getFileSrc account route', () => {
    test('serves the local blob store before the hub URL', async () => {
        const bytes = new Uint8Array([1, 2, 3])
        state.blobStore = createFakeBlobStore({ 'assets/acc-local.png': { data: bytes, mime: 'image/png' } })
        ;(forageStorage as any).isAccount = true

        const src = await getFileSrc('assets/acc-local.png')
        expect(src).toBe(`data:image/png;base64,${Buffer.from(bytes).toString('base64')}`)
    })

    test('falls back to the hub URL when the local store misses the key', async () => {
        state.blobStore = createFakeBlobStore({})
        ;(forageStorage as any).isAccount = true

        const src = await getFileSrc('assets/acc-remote.png')
        expect(src).toBe('https://hub.example/rs/assets/acc-remote.png')
    })

    test('serves the resolved file URL on Tauri before the hub URL', async () => {
        state.isTauri = true
        state.blobStore = createFakeBlobStore({ 'assets/acc-tauri.png': { data: new Uint8Array([1]), mime: 'image/png' } })
        ;(forageStorage as any).isAccount = true

        const src = await getFileSrc('assets/acc-tauri.png')
        expect(src).toBe('asset:///data/assets/acc-tauri.png')
    })
})

describe('getFileSrc tauri asset route', () => {
    test('resolves a native asset without initializing AutoStorage', async () => {
        state.isTauri = true
        state.blobStore = createFakeBlobStore({ 'assets/native.png': { data: new Uint8Array([7]), mime: 'image/png' } })
        const init = vi.spyOn(forageStorage, 'Init')

        await expect(getFileSrc('assets/native.png')).resolves.toBe('asset:///data/assets/native.png')
        expect(init).not.toHaveBeenCalled()
    })

    test('memoizes the resolved URL per key and invalidates it on saveAsset', async () => {
        state.isTauri = true
        state.blobStore = createFakeBlobStore({ 'assets/avatar.png': { data: new Uint8Array([7]), mime: 'image/png' } })

        expect(await getFileSrc('assets/avatar.png')).toBe('asset:///data/assets/avatar.png')
        expect(await getFileSrc('assets/avatar.png')).toBe('asset:///data/assets/avatar.png')
        expect(state.blobStore.resolveUrl).toHaveBeenCalledTimes(1)

        await saveAsset(new Uint8Array([8]), 'avatar', 'avatar.png')
        expect(state.blobStore.put).toHaveBeenCalledTimes(1)

        expect(await getFileSrc('assets/avatar.png')).toBe('asset:///data/assets/avatar.png')
        expect(state.blobStore.resolveUrl).toHaveBeenCalledTimes(2)
    })

    test('does not cache a missing asset resolution', async () => {
        state.isTauri = true
        state.blobStore = createFakeBlobStore({})

        expect(await getFileSrc('assets/ghost.png')).toBe('')
        state.blobStore = createFakeBlobStore({ 'assets/ghost.png': { data: new Uint8Array([1]), mime: 'image/png' } })
        expect(await getFileSrc('assets/ghost.png')).toBe('asset:///data/assets/ghost.png')
    })
})

describe('getFileSrc browser asset route', () => {
    test('caches the finished data URL so a hit skips stat and re-encoding', async () => {
        const bytes = new Uint8Array([9, 8, 7])
        state.blobStore = createFakeBlobStore({ 'assets/web.png': { data: bytes, mime: 'image/png' } })
        const expected = `data:image/png;base64,${Buffer.from(bytes).toString('base64')}`

        expect(await getFileSrc('assets/web.png')).toBe(expected)
        expect(await getFileSrc('assets/web.png')).toBe(expected)
        expect(state.blobStore.read).toHaveBeenCalledTimes(1)
        expect(state.blobStore.stat).toHaveBeenCalledTimes(1)
    })

    test('returns an empty string for a missing asset', async () => {
        state.blobStore = createFakeBlobStore({})
        expect(await getFileSrc('assets/web-missing.png')).toBe('')
    })

    test('clears retained data URLs when switching to the lower low-spec budget', async () => {
        const bytes = new Uint8Array([4, 5, 6])
        state.blobStore = createFakeBlobStore({ 'assets/profile.png': { data: bytes, mime: 'image/png' } })

        await getFileSrc('assets/profile.png')
        await getFileSrc('assets/profile.png')
        expect(state.blobStore.read).toHaveBeenCalledTimes(1)

        setRuntimePerformanceProfile('low-spec')

        await getFileSrc('assets/profile.png')
        expect(state.blobStore.read).toHaveBeenCalledTimes(2)
    })
})

describe('createThrottledSizeEstimator', () => {
    test('measures at most once per refresh window and reuses the last estimate', () => {
        vi.useFakeTimers()
        let value = 'aaaa'
        const read = vi.fn(() => value)
        const estimate = createThrottledSizeEstimator(read)

        expect(estimate()).toBe(JSON.stringify('aaaa').length)
        expect(read).toHaveBeenCalledTimes(1)

        value = 'a'.repeat(100)
        expect(estimate()).toBe(JSON.stringify('aaaa').length)
        expect(estimate()).toBe(JSON.stringify('aaaa').length)
        expect(read).toHaveBeenCalledTimes(1)

        vi.advanceTimersByTime(1000)
        expect(read).toHaveBeenCalledTimes(2)
        expect(estimate()).toBe(JSON.stringify('a'.repeat(100)).length)
    })

    test('reports zero for unserializable values instead of throwing', () => {
        const cyclic: { self?: unknown } = {}
        cyclic.self = cyclic
        const estimate = createThrottledSizeEstimator(() => cyclic)
        expect(estimate()).toBe(0)
    })
})
