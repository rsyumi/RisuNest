import { beforeEach, describe, expect, it, vi } from 'vitest'

const startup = vi.hoisted(() => {
    const calls: string[] = []
    const stopAfterAutoListen = new Error('stop after auto-listen wiring')
    const controller = {
        initialize: vi.fn(async () => undefined),
        prepare: vi.fn(async () => ({ phase: 'prepared' as const })),
        start: vi.fn(async () => ({ phase: 'running' as const })),
    }
    return {
        calls,
        controller,
        settings: {
            nativeFileLogEnabled: true,
            syncAutoListen: true,
            syncListenMethod: 'fixed-url' as const,
            syncFixedPort: 32145,
            syncPublicBaseUrl: 'https://sync.example.com',
        },
        stopAfterAutoListen,
        startAutoListen: vi.fn(async () => {
            calls.push('auto-listen')
            throw stopAfterAutoListen
        }),
        alertError: vi.fn(),
        bootFailure: vi.fn(),
    }
})

vi.mock('@tauri-apps/plugin-fs', () => ({
    BaseDirectory: { AppData: 'app-data' },
    exists: vi.fn(async () => true), mkdir: vi.fn(), readDir: vi.fn(), readFile: vi.fn(),
    remove: vi.fn(), writeFile: vi.fn(),
}))
vi.mock('@tauri-apps/api/webviewWindow', () => ({ getCurrentWebviewWindow: () => ({ maximize: vi.fn() }) }))
vi.mock('@tauri-apps/api/core', () => ({ convertFileSrc: vi.fn() }))
vi.mock('@tauri-apps/api/path', () => ({ appDataDir: vi.fn(), join: vi.fn() }))
vi.mock('svelte/store', async (importOriginal) => ({
    ...await importOriginal<typeof import('svelte/store')>(),
    get: () => false,
}))
vi.mock('./util', () => ({ changeFullscreen: vi.fn(), sleep: vi.fn() }))
vi.mock('./update', () => ({ checkRisuUpdate: vi.fn() }))
vi.mock('./gui/animation', () => ({ updateAnimationSpeed: vi.fn() }))
vi.mock('./gui/colorscheme', () => ({ updateColorScheme: vi.fn(), updateTextThemeAndCSS: vi.fn() }))
vi.mock('./observer.svelte', () => ({ startObserveDom: vi.fn() }))
vi.mock('./gui/guisize', () => ({ updateGuisize: vi.fn() }))
vi.mock('./hotkey', () => ({ initMobileGesture: vi.fn() }))
vi.mock('./process/modules', () => ({ moduleUpdate: vi.fn() }))
vi.mock('./drive/drive', () => ({ checkDriverInit: vi.fn() }))
vi.mock('./drive/accounter', () => ({ loadRisuAccountData: vi.fn() }))
vi.mock('./storage/risuSave', () => ({ decodeRisuSave: vi.fn() }))
vi.mock('./storage/remoteSaveCleanup', () => ({ getRemoteSaveCleanupAction: vi.fn(), getRemoteSavePayloadName: vi.fn() }))
vi.mock('./model/modellist', () => ({ registerModelDynamic: vi.fn() }))
vi.mock('./nativeScreenshotArchiveWriter', () => ({
    describeScreenshotPublicationError: vi.fn(), listenRecoveredAndroidScreenshotPublications: vi.fn(),
}))
vi.mock('./storage/nativePersistentMaintenance', () => ({
    restartNativeApp: vi.fn(), schedulePeriodicNativeSnapshot: vi.fn(),
}))
vi.mock('./storage/nativeFileJobs', () => ({
    NativeFileJobError: class extends Error {}, runNativeOfficialAccountSnapshotRestore: vi.fn(),
}))
vi.mock('./storage/androidRisuSaveRouteProduction.svelte', () => ({ registerAndroidRisuSaveRoute: vi.fn() }))
vi.mock('src/lang', async () => ({
    language: (await import('../lang/en')).languageEnglish,
    changeLanguage: vi.fn(),
}))
vi.mock('./platform', () => ({ isTauri: true, isTauriAndroid: false, isTauriDesktop: false }))
vi.mock('./storage/deviceSettings', () => ({ getDeviceSettings: () => startup.settings }))
vi.mock('./storage/sync/deviceSyncProduction', () => ({
    getProductionDeviceSyncController: vi.fn(() => {
        startup.calls.push('production-controller')
        return startup.controller
    }),
}))
vi.mock('./storage/sync/deviceSyncController', () => ({
    startDeviceSyncAutoListen: startup.startAutoListen,
}))
vi.mock('./nativeLog', () => ({
    setNativeLogFileEnabled: vi.fn(async () => { startup.calls.push('native-log') }),
}))
vi.mock('./storage/persistentStorageRuntime', () => ({
    initializePersistentStorage: vi.fn(async () => { startup.calls.push('persistent-storage') }),
    activateNativeAssetRepository: vi.fn(async () => null),
}))
vi.mock('./storage/nativeFileJobRecovery', () => ({
    shouldReconcileNativeFileJobs: vi.fn(() => false),
    reconcileNativeFileJobsBeforeBootstrap: vi.fn(async () => ({
        pendingRestoreAcknowledgements: [], pendingOfficialPublications: [],
    })),
    acknowledgeRecoveredNativeRestores: vi.fn(),
}))
vi.mock('./storage/persistentBootstrap', () => ({
    bootstrapPersistentDatabase: vi.fn(async () => {
        startup.calls.push('persistent-database')
        return { database: { characters: [], botPresets: [] }, profile: 'default', revision: 1 }
    }),
}))
vi.mock('./storage/database.svelte', () => ({ setDatabase: vi.fn(), getDatabase: vi.fn(() => ({})) }))
vi.mock('./storage/databasePreparation', () => ({
    prepareDatabaseForBootstrap: vi.fn(),
    checkNewFormat: vi.fn(), prepareDatabaseForPersistence: vi.fn(), preparePersistentRootForWorkingSet: vi.fn(),
    assignIds: vi.fn(),
}))
vi.mock('./storage/workingSetCatalog', () => ({
    createCatalogPresetWorkingSet: vi.fn(), hasIncompletePersistentWorkingSet: vi.fn(() => false),
    isCatalogCharacterStub: vi.fn(() => false), isCatalogPresetWorkingSet: vi.fn(() => false),
    projectCatalogWorkingSet: vi.fn(), projectCompleteScalableWorkingSet: vi.fn(),
}))
vi.mock('./storage/workingSetResidency', () => ({
    workingSetResidency: { clear: vi.fn(), markCharacterReleased: vi.fn(), reconcileConversationResidency: vi.fn() },
}))
vi.mock('./storage/persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => ({ store: {}, revision: 1, flushPendingData: vi.fn() }),
    initializeActiveWorkingSet: vi.fn(), configurePersistentDataRuntime: vi.fn(),
    hasPendingOfficialPublication: vi.fn(() => false), publishCurrentOfficialRevision: vi.fn(),
}))
vi.mock('./plugins/plugins.svelte', () => ({
    loadPlugins: vi.fn(), pluginCompatibility: { initialize: vi.fn(), profile: 'default' },
}))
vi.mock('./plugins/pluginCompatibility', () => ({ shouldProjectScalableWorkingSet: vi.fn(() => false) }))
vi.mock('./storage/accountStorage', () => ({
    AccountStorage: class { readItem = vi.fn() }, resetAccountStorageSession: vi.fn(),
}))
vi.mock('./storage/nativeAppKv', () => ({ createNativeAppKv: () => null, createNativeAppKvStringStorage: vi.fn() }))
vi.mock('./storage/sync/officialAccountSnapshot', () => ({
    OfficialAccountSnapshotAdapter: class {}, createOfficialAssociationMarkers: () => ({}),
}))
vi.mock('./storage/sync/officialAccountBootstrap', () => ({
    initializeOfficialAccountBootstrap: vi.fn(async () => {
        startup.calls.push('official-account')
        return { officialEnabled: false }
    }),
    publishOfficialRevisionIfChanged: vi.fn(),
}))
vi.mock('./storage/sync/officialAssetLedger', () => ({
    createAccountScopedOfficialAssetLedger: () => ({ reset: vi.fn() }),
}))
vi.mock('./storage/accountAssetAccess', () => ({
    configureOfficialAccountAssetReader: vi.fn(), createStructuredAccountAssetReader: vi.fn(),
}))
vi.mock('./storage/sync/nativeOfficialAccountFlow', () => ({
    configureNativeOfficialAccountFlow: vi.fn(), createNativeOfficialAccountFlowService: vi.fn(),
    nativeOfficialAccountKeys: { credential: 'credential', association: 'association', assetLedger: 'asset-ledger' },
    normalizeNativeOfficialAccountCredential: vi.fn(),
}))
vi.mock('./storage/sync/nativeOfficialPublicationJob', () => ({
    createNativeOfficialPublicationJobPublisher: vi.fn(() => ({})),
}))
vi.mock('./storage/sync/nativeOfficialPublicationRecovery', () => ({
    createNativeOfficialPublicationRecovery: vi.fn(),
}))
vi.mock('./storage/platformBlobStore', () => ({ resolveBlobStore: vi.fn() }))
vi.mock('./storage/sync/syncConflictBackup', () => ({ getSyncConflictBackupStore: vi.fn() }))
vi.mock('./storage/sync/syncConflictSummary', () => ({ formatNameList: vi.fn(), summarizePinnedSyncConflict: vi.fn() }))
vi.mock('./storage/persistentRecordIterator', () => ({ withPersistentRevisionLease: vi.fn() }))
vi.mock('./process/coldstorage.svelte', () => ({
    getAccountColdStorageItem: vi.fn(), getColdStorageItem: vi.fn(), makeColdData: vi.fn(),
    setAccountColdStorageItem: vi.fn(),
}))
vi.mock('./globalApi.svelte', () => ({
    forageStorage: { isAccount: false, setAccountModeForSession: vi.fn() }, saveDb: vi.fn(),
    getUncleanables: vi.fn(), getBasename: vi.fn(), invalidateAssetSourceCache: vi.fn(), setUsingSw: vi.fn(),
}))
vi.mock('./stores.svelte', () => ({
    MobileGUI: { set: vi.fn() }, botMakerMode: { set: vi.fn() }, selectedCharID: { set: vi.fn() },
    loadedStore: {}, DBState: {}, LoadingStatusState: { text: '' },
    bootFailure: { set: (...args: unknown[]) => startup.bootFailure(...args) },
}))
vi.mock('./alert', () => ({
    alertConfirm: vi.fn(), alertError: (...args: unknown[]) => startup.alertError(...args),
    alertInput: vi.fn(), alertLogin: vi.fn(), alertMd: vi.fn(), alertNormal: vi.fn(),
    alertSelect: vi.fn(), alertTOS: vi.fn(), waitAlert: vi.fn(),
}))
vi.mock('./characterCards', () => ({ characterURLImport: vi.fn(), hubURL: 'https://hub.invalid' }))
vi.mock('./storage/androidSafBridge', () => ({ isAndroidSafFileJobsEnabled: vi.fn(() => false) }))
vi.mock('./storage/lifecycleCommit', () => ({ registerLifecycleCommitListeners: vi.fn() }))

describe('device sync bootstrap wiring', () => {
    beforeEach(() => {
        startup.calls.length = 0
        startup.startAutoListen.mockClear()
        startup.controller.initialize.mockClear()
        startup.alertError.mockClear()
        startup.bootFailure.mockClear()
    })

    it('connects the production controller to auto-listen after native data initialization', async () => {
        const { loadData } = await import('./bootstrap')

        await loadData()

        expect(startup.calls).toEqual([
            'native-log',
            'persistent-storage',
            'persistent-database',
            'official-account',
            'production-controller',
            'auto-listen',
        ])
        expect(startup.controller.initialize).toHaveBeenCalledOnce()
        expect(startup.startAutoListen).toHaveBeenCalledWith(
            startup.settings,
            expect.objectContaining({ controller: startup.controller }),
        )
        expect(startup.alertError).toHaveBeenCalledWith(startup.stopAfterAutoListen)
        expect(startup.bootFailure).toHaveBeenCalledWith({
            kind: 'unknown',
            message: startup.stopAfterAutoListen.message,
            stage: 'device-sync',
        })
        const { LoadingStatusState } = await import('./stores.svelte')
        expect(LoadingStatusState.startedAt).toBeNull()
    })
})
