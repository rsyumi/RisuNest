import { afterEach, beforeEach, describe, expect, test, vi } from 'vitest'

const state = vi.hoisted(() => ({
    isTauri: false,
    isNodeServer: false,
    database: { usePlainFetch: true },
    isTauriMobile: false,
    blobStore: null as any,
}))

vi.mock('./platform', () => ({
    get isTauri() {
        return state.isTauri
    },
    get isTauriMobile() {
        return state.isTauriMobile
    },
    get isNodeServer() {
        return state.isNodeServer
    },
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
        async Init() {
            /* no-op */
        }
        async getItem() {
            return null
        }
        async setItem() {
            /* no-op */
        }
        async removeItem() {
            /* no-op */
        }
        async keys() {
            return []
        }
    },
}))
vi.mock('./characterCards', () => ({
    hubURL: 'https://hub.example',
    // `/rs/` is a Realm-classified path (scripts/realmBlocklist.mjs), so the
    // account asset route must build it from realmHubURL, not hubURL.
    realmHubURL: 'https://realm-hub.example',
    characterURLImport: vi.fn(),
}))
vi.mock('./util', () => ({
    changeFullscreen: vi.fn(),
    sleep: async () => undefined,
}))
vi.mock('./storage/database.svelte', () => ({
    getDatabase: () => state.database,
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
    alertTOS: vi.fn(), alertRisuServiceTOS: vi.fn(),
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
    defaultJailbreak: '',
    defaultMainPrompt: '',
    oldJailbreak: '',
    oldMainPrompt: '',
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
vi.mock('./parser/parser.svelte', () => ({
    hasher: vi.fn(async () => 'hashed'),
}))
vi.mock('./drive/accounter', () => ({ loadRisuAccountData: vi.fn() }))
vi.mock('./update', () => ({ checkRisuUpdate: vi.fn() }))
vi.mock('./observer.svelte', () => ({ startObserveDom: vi.fn() }))
vi.mock('./characters', () => ({ updateLorebooks: vi.fn() }))
vi.mock('./hotkey', () => ({ initMobileGesture: vi.fn() }))
vi.mock('./gui/animation', () => ({ updateAnimationSpeed: vi.fn() }))
vi.mock('./gui/colorscheme', () => ({
    updateColorScheme: vi.fn(),
    updateTextThemeAndCSS: vi.fn(),
}))
vi.mock('./gui/guisize', () => ({ updateGuisize: vi.fn() }))
vi.mock('src/lang', () => ({ language: {} }))
vi.mock('streamsaver', () => ({ default: { createWriteStream: vi.fn() } }))
vi.mock('@tauri-apps/plugin-fs', () => ({
    writeFile: vi.fn(),
    readFile: vi.fn(),
    exists: vi.fn(),
    mkdir: vi.fn(),
    readDir: vi.fn(),
    remove: vi.fn(),
    BaseDirectory: { Download: 1, AppData: 2 },
}))
vi.mock('@tauri-apps/api/core', () => ({
    convertFileSrc: (path: string) => `asset://${path}`,
}))
vi.mock('@tauri-apps/api/path', () => ({
    appDataDir: vi.fn(async () => '/data'),
    join: vi.fn(async (...parts: string[]) => parts.join('/')),
}))
vi.mock('@tauri-apps/plugin-shell', () => ({ open: vi.fn() }))
vi.mock('@tauri-apps/plugin-dialog', () => ({ save: vi.fn() }))
vi.mock('@tauri-apps/api/webviewWindow', () => ({
    getCurrentWebviewWindow: vi.fn(() => ({})),
}))
vi.mock('@tauri-apps/plugin-http', () => ({ fetch: vi.fn() }))

import { fetchNative, globalFetch } from './globalApi.svelte'

describe('network requests without application URL restrictions', () => {
    beforeEach(() => {
        state.isTauri = false
        state.isNodeServer = false
        state.database.usePlainFetch = true
        vi.stubGlobal(
            'fetch',
            vi.fn(
                async () => new Response('{"fixture":true}', { status: 200 }),
            ),
        )
    })

    afterEach(() => vi.unstubAllGlobals())

    test.each([undefined, 'local_network'] as const)(
        'globalFetch attempts browser LAN requests (%s)',
        async (networkRoute) => {
            const url = 'http://localhost:11434/api/chat'
            const response = await globalFetch(url, {
                method: 'GET',
                networkRoute,
            })
            expect(response).toMatchObject({
                ok: true,
                status: 200,
                data: { fixture: true },
            })
            expect(fetch).toHaveBeenCalledWith(
                new URL(url),
                expect.objectContaining({ method: 'GET' }),
            )
        },
    )

    test.each([undefined, 'local_network'] as const)(
        'nativeFetch routes browser LAN requests directly even with the public proxy enabled (%s)',
        async (networkRoute) => {
            state.database.usePlainFetch = false
            const url = 'http://192.168.1.2:8080/v1/chat'
            const response = await fetchNative(url, {
                method: 'GET',
                networkRoute,
            })
            expect(response.status).toBe(200)
            expect(fetch).toHaveBeenCalledWith(url, expect.anything())
        },
    )

    test('nativeFetch accepts omitted options and empty POST bodies', async () => {
        await fetchNative('https://api.example.invalid/')
        expect(fetch).toHaveBeenLastCalledWith(
            'https://api.example.invalid/',
            expect.objectContaining({ method: 'GET', body: undefined }),
        )
        await fetchNative('https://api.example.invalid/', { method: 'POST' })
        expect(fetch).toHaveBeenLastCalledWith(
            'https://api.example.invalid/',
            expect.objectContaining({ method: 'POST', body: undefined }),
        )
    })

    test('nativeFetch preserves a DELETE payload and supports PATCH', async () => {
        for (const method of ['DELETE', 'PATCH']) {
            await fetchNative('https://api.example.invalid/', {
                method,
                body: 'fixture',
            })
            expect(fetch).toHaveBeenLastCalledWith(
                'https://api.example.invalid/',
                expect.objectContaining({
                    method,
                    body: new TextEncoder().encode('fixture'),
                }),
            )
        }
    })

    test('nativeFetch retains implicit POST with a body and does not mutate caller options', async () => {
        const options = { body: 'fixture' }
        await fetchNative('https://api.example.invalid/', options)
        expect(fetch).toHaveBeenCalledWith(
            'https://api.example.invalid/',
            expect.objectContaining({ method: 'POST' }),
        )
        expect(options).toEqual({ body: 'fixture' })
    })

    test('forwards browser network failures instead of replacing them with a URL policy error', async () => {
        const error = new TypeError('synthetic browser network error')
        vi.mocked(fetch).mockRejectedValue(error)
        await expect(
            fetchNative('http://localhost:8080/', { method: 'GET' }),
        ).rejects.toBe(error)
    })
})
