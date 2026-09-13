import { describe, expect, it, vi } from 'vitest'

vi.mock('@tauri-apps/plugin-fs', () => ({
    BaseDirectory: { AppData: 'app-data' },
    exists: vi.fn(), mkdir: vi.fn(), readDir: vi.fn(), readFile: vi.fn(),
    remove: vi.fn(), writeFile: vi.fn(),
}))
vi.mock('@tauri-apps/api/webviewWindow', () => ({ getCurrentWebviewWindow: () => ({ maximize: vi.fn() }) }))
vi.mock('@tauri-apps/api/core', () => ({ convertFileSrc: vi.fn() }))
vi.mock('@tauri-apps/api/path', () => ({ appDataDir: vi.fn(), join: vi.fn() }))
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
vi.mock('src/lang', () => ({ language: {} }))
vi.mock('./platform', () => ({ isTauri: true, isTauriAndroid: false, isTauriDesktop: false }))
vi.mock('./storage/deviceSettings', () => ({ getDeviceSettings: () => ({ nativeFileLogEnabled: false }) }))
vi.mock('./nativeLog', () => ({ setNativeLogFileEnabled: vi.fn() }))
vi.mock('./storage/persistentStorageRuntime', () => ({
    initializePersistentStorage: vi.fn(), activateNativeAssetRepository: vi.fn(),
}))
vi.mock('./storage/nativeFileJobRecovery', () => ({
    shouldReconcileNativeFileJobs: vi.fn(() => false),
    reconcileNativeFileJobsBeforeBootstrap: vi.fn(),
    acknowledgeRecoveredNativeRestores: vi.fn(),
}))
vi.mock('./storage/persistentBootstrap', () => ({ bootstrapPersistentDatabase: vi.fn() }))
vi.mock('./storage/database.svelte', () => ({ setDatabase: vi.fn(), getDatabase: vi.fn(() => ({})) }))
vi.mock('./storage/databasePreparation', () => ({
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
    initializeOfficialAccountBootstrap: vi.fn(), publishOfficialRevisionIfChanged: vi.fn(),
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
    loadedStore: {}, DBState: {}, LoadingStatusState: { text: '' }, bootFailure: { set: vi.fn() },
}))
vi.mock('./alert', () => ({
    alertConfirm: vi.fn(), alertError: vi.fn(), alertInput: vi.fn(), alertLogin: vi.fn(), alertMd: vi.fn(),
    alertNormal: vi.fn(), alertSelect: vi.fn(), alertTOS: vi.fn(), alertRisuServiceTOS: vi.fn(), waitAlert: vi.fn(),
}))
vi.mock('./characterCards', () => ({ characterURLImport: vi.fn(), hubURL: 'https://hub.invalid' }))
vi.mock('./storage/androidSafBridge', () => ({ isAndroidSafFileJobsEnabled: vi.fn(() => false) }))
vi.mock('./storage/lifecycleCommit', () => ({ registerLifecycleCommitListeners: vi.fn() }))

import { classifyBootFailure } from './bootstrap'

describe('classifyBootFailure', () => {
    it('names an incompatible persistent schema wherever it is thrown', () => {
        expect(classifyBootFailure(
            new Error('unsupported persistent schema version 17'),
            'persistent-storage',
        )).toEqual({
            kind: 'schema-unsupported',
            message: 'unsupported persistent schema version 17',
            stage: 'persistent-storage',
        })
        expect(classifyBootFailure(
            new Error('unsupported persistent schema version 17'),
            'plugins',
        ).kind).toBe('schema-unsupported')
    })

    it.each(['persistent-storage', 'persistent-database'])(
        'treats a failure in the %s stage as a store that could not be opened',
        (stage) => {
            expect(classifyBootFailure(new Error('disk I/O error'), stage)).toEqual({
                kind: 'store-open',
                message: 'disk I/O error',
                stage,
            })
        },
    )

    it.each([undefined, 'startup', 'plugins', 'ui-state'])(
        'falls back to an unknown failure for the %s stage',
        (stage) => {
            expect(classifyBootFailure(new Error('boom'), stage)).toEqual({
                kind: 'unknown',
                message: 'boom',
                stage,
            })
        },
    )

    it('reads a message out of a raw string and a native rejection object', () => {
        expect(classifyBootFailure('unsupported persistent schema version 17')).toEqual({
            kind: 'schema-unsupported',
            message: 'unsupported persistent schema version 17',
            stage: undefined,
        })
        expect(classifyBootFailure(
            { code: 'store-error', message: 'unsupported persistent schema version 17' },
            'persistent-database',
        ).kind).toBe('schema-unsupported')
        expect(classifyBootFailure(null, 'plugins')).toEqual({
            kind: 'unknown',
            message: 'null',
            stage: 'plugins',
        })
    })
})
