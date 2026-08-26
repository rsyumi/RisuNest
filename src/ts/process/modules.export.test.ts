import { readFileSync } from 'node:fs'
import { beforeEach, describe, expect, it, vi } from 'vitest'

const mocks = vi.hoisted(() => ({
    compressImage: vi.fn(async (_data: Uint8Array) => new Uint8Array([0xde, 0xad, 0xbe, 0xef])),
    readImage: vi.fn(),
    saveAsset: vi.fn(async (_data: Uint8Array) => ''),
}))

vi.mock('src/lang', () => ({
    language: {
        errors: { noData: 'no data' },
        successExport: 'exported',
    },
}))
vi.mock('../alert', () => ({
    alertClear: vi.fn(),
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertModuleSelect: vi.fn(),
    alertNormal: vi.fn(),
    alertStore: { set: vi.fn() },
    alertWait: vi.fn(),
}))
vi.mock('../storage/database.svelte', () => ({
    appSubVer: '',
    appVer: '0.0.0',
    defaultSdDataFunc: () => [],
    getCurrentCharacter: vi.fn(),
    getCurrentChat: vi.fn(),
    getDatabase: vi.fn(),
    setCurrentCharacter: vi.fn(),
    setDatabase: vi.fn(),
}))
vi.mock('../globalApi.svelte', async (importOriginal) => {
    const actual = await importOriginal<typeof import('../globalApi.svelte')>()
    return {
        ...actual,
        downloadFile: vi.fn(),
        forageStorage: {},
        readImage: mocks.readImage,
        saveAsset: mocks.saveAsset,
    }
})
vi.mock('../util', () => ({
    changeFullscreen: vi.fn(),
    checkPersonaBinded: vi.fn(),
    selectSingleFile: vi.fn(),
    sleep: vi.fn(),
}))
vi.mock('uuid', () => ({ v4: () => 'roundtrip-module-id' }))
vi.mock('./lorebook.svelte', () => ({ convertExternalLorebook: vi.fn() }))
vi.mock('../media', () => ({ compressImage: mocks.compressImage }))
vi.mock('../stores.svelte', () => ({
    bodyIntercepterStore: [],
    botMakerMode: { set: vi.fn() },
    DBState: { db: { modules: [] } },
    HideIconStore: { set: vi.fn() },
    loadedStore: { set: vi.fn() },
    LoadingStatusState: {},
    MobileGUI: { set: vi.fn() },
    moduleBackgroundEmbedding: { set: vi.fn() },
    ReloadGUIPointer: { set: vi.fn() },
    selectedCharID: { set: vi.fn() },
    selIdState: { selId: 0 },
}))
vi.mock('../interchangeability', () => ({
    convertCharacterToModule: vi.fn(),
    convertModuleToCharacter: vi.fn(),
}))
vi.mock('../characterCards', () => ({
    characterURLImport: vi.fn(),
    exportCharacterCard: vi.fn(),
    hubURL: 'https://hub.invalid',
    importCharacterProcess: vi.fn(),
}))
vi.mock('../parser/parser.svelte', () => ({ hasher: vi.fn(async () => 'hashed') }))
vi.mock('../gui/colorscheme', () => ({
    updateColorScheme: vi.fn(),
    updateTextThemeAndCSS: vi.fn(),
}))
vi.mock('../platform', () => ({ isNodeServer: false, isTauri: false, isTauriMobile: false }))
vi.mock('../storage/platformBlobStore', () => ({
    configureBlobStoreStorageProvider: vi.fn(),
    readBlobForFacade: vi.fn(),
    resolveBlobStore: vi.fn(),
}))
vi.mock('../storage/autoStorage', () => ({
    AutoStorage: class {
        realStorage = {}
        async Init() {}
    },
}))
vi.mock('../plugins/plugins.svelte', () => ({ loadPlugins: vi.fn() }))
vi.mock('../drive/drive', () => ({ checkDriverInit: vi.fn() }))
vi.mock('../drive/accounter', () => ({ loadRisuAccountData: vi.fn() }))
vi.mock('../update', () => ({ checkRisuUpdate: vi.fn() }))
vi.mock('../observer.svelte', () => ({ startObserveDom: vi.fn() }))
vi.mock('../characters', () => ({ updateLorebooks: vi.fn() }))
vi.mock('../hotkey', () => ({ initMobileGesture: vi.fn() }))
vi.mock('../gui/animation', () => ({ updateAnimationSpeed: vi.fn() }))
vi.mock('../gui/guisize', () => ({ updateGuisize: vi.fn() }))
vi.mock('../kei/backup', () => ({ saveDbKei: vi.fn() }))
vi.mock('../storage/risuSave', () => ({ decodeRisuSave: vi.fn() }))
vi.mock('../storage/defaultPrompts', () => ({
    defaultJailbreak: '',
    defaultMainPrompt: '',
    oldJailbreak: '',
    oldMainPrompt: '',
}))
vi.mock('./coldstorage.svelte', () => ({ getColdStorageItem: vi.fn(), makeColdData: vi.fn() }))
vi.mock('./coldstorageData', () => ({
    listCharacterResources: vi.fn(() => []),
    listDatabaseRootResources: vi.fn(() => []),
    replaceCharacterResources: vi.fn(),
    replaceDatabaseRootResources: vi.fn(),
}))
vi.mock('../storage/persistentDataRuntime.svelte', () => ({
    activateConversation: vi.fn(),
    configurePersistentDataRuntime: vi.fn(),
    markPersistentDataDirty: vi.fn(),
    replacePersistentDatabase: vi.fn(),
}))
vi.mock('../storage/persistentSaveNotifications', () => ({
    createPersistentSaveObserverInstallation: () => ({ install: vi.fn() }),
    installPersistentSaveNotifications: vi.fn(),
}))
vi.mock('../storage/nodeStorage', () => ({ getNodeServerProxyAuth: vi.fn() }))
vi.mock('../storage/databasePreparation', () => ({ checkCharOrder: vi.fn() }))
vi.mock('../storage/accountAssetAccess', () => ({
    readActiveAsset: vi.fn(),
    storeActiveAsset: vi.fn(),
}))
vi.mock('../chatMessageUi', () => ({
    captureChatMessageTarget: vi.fn(),
    captureChatMessageTargetById: vi.fn(),
    resolveRetainedChatMessageTarget: vi.fn(),
}))
vi.mock('../network/tauriHttpStream', () => ({ fetchTauriHttpStream: vi.fn() }))

import { exportModuleLegacy, readModule, type RisuModule } from './modules'

describe('legacy module export', () => {
    beforeEach(() => {
        vi.clearAllMocks()
        const rpackMap = readFileSync('src/ts/rpack/rpack_map.bin')
        vi.stubGlobal('fetch', vi.fn(async () => ({
            arrayBuffer: async () => rpackMap.buffer.slice(
                rpackMap.byteOffset,
                rpackMap.byteOffset + rpackMap.byteLength,
            ),
        })))
    })

    it('roundtrips ordinary asset bytes and preserves asset metadata without image compression', async () => {
        const firstBytes = new Uint8Array([0x00, 0xff, 0x13, 0x7a, 0x80, 0x42])
        const secondBytes = new Uint8Array([0x91, 0x04, 0xcc, 0x2d, 0x7f])
        mocks.readImage.mockImplementation(async (source: string) => {
            if (source === 'asset://source-a') return firstBytes
            if (source === 'asset://source-b') return secondBytes
            throw new Error(`Unexpected asset source: ${source}`)
        })
        mocks.saveAsset
            .mockResolvedValueOnce('asset://roundtrip-a')
            .mockResolvedValueOnce('asset://roundtrip-b')
        const module: RisuModule = {
            id: 'source-module-id',
            name: 'Byte exact module',
            description: 'Legacy asset roundtrip',
            assets: [
                ['ordinary-asset-a', 'asset://source-a', 'bin'],
                ['ordinary-asset-b', 'asset://source-b', 'dat'],
            ],
        }

        const exported = await exportModuleLegacy(module, { alertEnd: false, saveData: false })
        const imported = await readModule(Buffer.from(exported))

        expect(mocks.compressImage).not.toHaveBeenCalled()
        expect(mocks.readImage.mock.calls.map(([source]) => source)).toEqual([
            'asset://source-a',
            'asset://source-b',
        ])
        expect(mocks.saveAsset).toHaveBeenCalledTimes(2)
        expect(mocks.saveAsset.mock.calls.map(([data]) => Array.from(data))).toEqual([
            Array.from(firstBytes),
            Array.from(secondBytes),
        ])
        expect(imported.assets).toEqual([
            ['ordinary-asset-a', 'asset://roundtrip-a', 'bin'],
            ['ordinary-asset-b', 'asset://roundtrip-b', 'dat'],
        ])
    })
})
