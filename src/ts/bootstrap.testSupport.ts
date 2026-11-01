import { vi, type Mock } from 'vitest'
import type { WebviewWindow } from '@tauri-apps/api/webviewWindow'

// Shared vi.mock module factories that cut the import graph of bootstrap.ts for loadData tests.
// Factories that follow the test's startup state take it as their argument. Consume as:
//   vi.mock('./util', async () => (await import('./bootstrap.testSupport')).utilModule())

/** The hoisted state a loadData test drives its stand-ins from. */
export interface BootstrapTestState {
    native: boolean
    exclusions: ReturnType<typeof import('./storage/deviceSettings')['getStartupExclusions']>
    database: any
    reachUi: boolean
    takeRendererRecovery: Mock<() => Promise<boolean>>
    credential: unknown
    readVault: Mock
    reconcile: Mock
    external: Mock
    transports: Mock
    stages: Mock
    serverPaused: boolean
    lwwRunning: boolean
    exitDrain: Mock
    serverDrain: Mock
    stop: Error
    markers: { getItem: Mock<(key: string) => string | null>, setItem: Mock, removeItem: Mock, flush: Mock<() => Promise<void>> }
}

/** Types a stand-in that carries only the members these paths touch; each member name must exist on the real type. */
function stub<T>(members: { [K in keyof T]?: unknown }): T {
    return members as T
}

/** The entry point's view of bootstrap.ts, for tests of main and normalMain. */
export function bootstrapModule() {
    return { loadData: vi.fn() } satisfies typeof import('./bootstrap')
}

export function tauriFsModule() {
    return {
        BaseDirectory: stub<typeof import('@tauri-apps/plugin-fs')['BaseDirectory']>({ AppData: 'app-data' }),
        exists: vi.fn(), mkdir: vi.fn(), readDir: vi.fn(), readFile: vi.fn(),
        remove: vi.fn(), writeFile: vi.fn(),
    } satisfies Partial<typeof import('@tauri-apps/plugin-fs')>
}

export function webviewWindowModule() {
    return { getCurrentWebviewWindow: () => stub<WebviewWindow>({ maximize: vi.fn() }) } satisfies Partial<typeof import('@tauri-apps/api/webviewWindow')>
}

export function tauriCoreModule() {
    return { convertFileSrc: vi.fn(), invoke: vi.fn() } satisfies Partial<typeof import('@tauri-apps/api/core')>
}

export function tauriPathModule() {
    return { join: vi.fn() } satisfies Partial<typeof import('@tauri-apps/api/path')>
}

export function utilModule() {
    return { changeFullscreen: vi.fn(), sleep: vi.fn() } satisfies Partial<typeof import('./util')>
}

export function animationModule() {
    return { updateAnimationSpeed: vi.fn() } satisfies Partial<typeof import('./gui/animation')>
}

export function colorschemeModule(startup: BootstrapTestState) {
    return {
        updateColorScheme: () => { if (!startup.reachUi) throw startup.stop }, updateTextThemeAndCSS: vi.fn(),
    } satisfies Partial<typeof import('./gui/colorscheme')>
}

export function observerModule() {
    return { startObserveDom: vi.fn() } satisfies Partial<typeof import('./observer.svelte')>
}

export function guisizeModule() {
    return { updateGuisize: vi.fn() } satisfies Partial<typeof import('./gui/guisize')>
}

export function hotkeyModule() {
    return { initMobileGesture: vi.fn() } satisfies Partial<typeof import('./hotkey')>
}

export function modulesModule() {
    return { moduleUpdate: vi.fn() } satisfies Partial<typeof import('./process/modules')>
}

export function accounterModule() {
    return { loadRisuAccountData: vi.fn() } satisfies Partial<typeof import('./drive/accounter')>
}

export function remoteSaveCleanupModule() {
    return { getRemoteSaveCleanupAction: vi.fn(), getRemoteSavePayloadName: vi.fn() } satisfies Partial<typeof import('./storage/remoteSaveCleanup')>
}

export function modellistModule() {
    return { registerModelDynamic: vi.fn() } satisfies Partial<typeof import('./model/modellist')>
}

export function nativeScreenshotArchiveWriterModule() {
    return {
        describeScreenshotPublicationError: vi.fn(), listenRecoveredAndroidScreenshotPublications: vi.fn(),
    } satisfies Partial<typeof import('./nativeScreenshotArchiveWriter')>
}

export function nativePersistentMaintenanceModule() {
    return {
        checkpointNativePersistentStore: vi.fn(), restartNativeApp: vi.fn(), schedulePeriodicNativeSnapshot: vi.fn(),
    } satisfies Partial<typeof import('./storage/nativePersistentMaintenance')>
}

export function nativeFileJobsModule() {
    return {
        NativeFileJobError: class extends Error {} as unknown as typeof import('./storage/nativeFileJobs')['NativeFileJobError'],
        runNativeOfficialAccountSnapshotRestore: vi.fn(),
    } satisfies Partial<typeof import('./storage/nativeFileJobs')>
}

export function androidRisuSaveRouteModule() {
    return { registerAndroidRisuSaveRoute: vi.fn() } satisfies Partial<typeof import('./storage/androidRisuSaveRouteProduction.svelte')>
}

export function iosFilesModule() {
    return { reportInterruptedIOSBackupSourcesAtStart: vi.fn() } satisfies Partial<typeof import('./storage/iosFiles')>
}

export function deviceSettingsModule(startup: BootstrapTestState) {
    type DeviceSettings = typeof import('./storage/deviceSettings')
    return {
        getStartupExclusions: () => startup.exclusions,
        loadDeviceSettings: () => stub<ReturnType<DeviceSettings['loadDeviceSettings']>>({ nativeFileLogEnabled: false }),
        reloadDeviceSettings: () => stub<ReturnType<DeviceSettings['reloadDeviceSettings']>>({ nativeFileLogEnabled: false }),
    } satisfies Partial<DeviceSettings>
}

export function storeCompactionModule() {
    return {
        compactRequestedStoreAtStart: vi.fn(async () => null), showStoreCompactionNotice: vi.fn(async () => {}),
    } satisfies Partial<typeof import('./storage/storeCompaction')>
}

export function persistentStorageRuntimeModule() {
    return {
        initializePersistentStorage: vi.fn(),
    } satisfies Partial<typeof import('./storage/persistentStorageRuntime')>
}

export function nativeFileJobRecoveryModule() {
    type Recovery = typeof import('./storage/nativeFileJobRecovery')
    return {
        shouldReconcileNativeFileJobs: vi.fn(() => false),
        reconcileNativeFileJobsBeforeBootstrap: vi.fn(async () => stub<Awaited<ReturnType<Recovery['reconcileNativeFileJobsBeforeBootstrap']>>>({
            pendingRestoreAcknowledgements: [], pendingOfficialPublications: [], interruptedRestores: [],
        })),
        acknowledgeRecoveredNativeRestores: vi.fn(),
    } satisfies Partial<Recovery>
}

export function persistentBootstrapModule(startup: BootstrapTestState) {
    return { bootstrapPersistentDatabase: async () => ({ database: startup.database, revision: 1, source: 'persistent' }) } satisfies Partial<typeof import('./storage/persistentBootstrap')>
}

export function databaseModule(startup: BootstrapTestState) {
    return { setDatabase: (value: unknown) => { startup.database = value }, getDatabase: () => startup.database } satisfies Partial<typeof import('./storage/database.svelte')>
}

export function databasePreparationModule() {
    return {
        checkNewFormat: vi.fn(), prepareDatabaseForPersistence: vi.fn(), prepareDatabaseForBootstrap: vi.fn(), preparePersistentRootForWorkingSet: vi.fn(),
        assignIds: vi.fn(),
    } satisfies Partial<typeof import('./storage/databasePreparation')>
}

export function workingSetCatalogModule() {
    return {
        createCatalogPresetWorkingSet: vi.fn(), hasIncompletePersistentWorkingSet: vi.fn(() => false),
        isCatalogCharacterStub: vi.fn(() => false), isCatalogPresetWorkingSet: vi.fn(() => false),
        isWorkingSetCharacterStub: vi.fn(() => false),
        projectCatalogWorkingSet: vi.fn(), projectCompleteScalableWorkingSet: vi.fn(),
    } satisfies Partial<typeof import('./storage/workingSetCatalog')>
}

export function workingSetResidencyModule() {
    type Residency = typeof import('./storage/workingSetResidency')
    return {
        workingSetResidency: stub<Residency['workingSetResidency']>({
            clear: vi.fn(), markCharacterReleased: vi.fn(), reconcileConversationResidency: vi.fn(),
        }),
    } satisfies Partial<Residency>
}

export function persistentDataRuntimeModule() {
    type Runtime = ReturnType<typeof import('./storage/persistentDataRuntime.svelte')['getPersistentDataRuntime']>
    return {
        getPersistentDataRuntime: () => stub<Runtime>({ store: {}, revision: 1, flushPendingData: vi.fn(), expirePersistentTrash: vi.fn(async () => {}),
            capturePersistentMutationToken: vi.fn(async () => ({ revision: 5 })), acquireDestructiveReplacementFence: vi.fn(async () => ({ revision: 5 })) }),
        initializeActiveWorkingSet: vi.fn(), configurePersistentDataRuntime: vi.fn(),
        hasPendingOfficialPublication: vi.fn(() => false), publishCurrentOfficialRevision: vi.fn(),
    } satisfies Partial<typeof import('./storage/persistentDataRuntime.svelte')>
}

export function pluginsModule() {
    return {
        keepPluginsOffForThisStart: vi.fn(), afterPluginsStarted: vi.fn(() => () => {}), loadPlugins: vi.fn(), loadPluginsAfterAuthoritativeRestore: vi.fn(),
    } satisfies Partial<typeof import('./plugins/plugins.svelte')>
}

export function accountStorageModule() {
    return {
        AccountStorage: class { readItem = vi.fn() } as unknown as typeof import('./storage/accountStorage')['AccountStorage'],
        resetAccountStorageSession: vi.fn(),
    } satisfies Partial<typeof import('./storage/accountStorage')>
}

export function nativeAccountCredentialModule(startup: BootstrapTestState) {
    return {
        createNativeAccountCredentialVault: () => stub<ReturnType<typeof import('./storage/nativeAccountCredential')['createNativeAccountCredentialVault']>>({
            read: startup.readVault,
        }),
    } satisfies Partial<typeof import('./storage/nativeAccountCredential')>
}

export function nativeDeviceSettingsModule() {
    type NativeDeviceSettings = typeof import('./storage/nativeDeviceSettings')
    return {
        createNativeDeviceSettings: () => stub<ReturnType<NativeDeviceSettings['createNativeDeviceSettings']>>({ get: vi.fn(async () => null), set: vi.fn() }),
        createNativeDeviceSettingsBag: vi.fn(),
    } satisfies Partial<NativeDeviceSettings>
}

export function officialAccountSnapshotModule() {
    type Snapshot = typeof import('./storage/sync/officialAccountSnapshot')
    return {
        OfficialAccountSnapshotAdapter: class {} as unknown as Snapshot['OfficialAccountSnapshotAdapter'],
        createOfficialAssociationMarkers: () => stub<ReturnType<Snapshot['createOfficialAssociationMarkers']>>({}),
    } satisfies Partial<Snapshot>
}

export function officialAccountBootstrapModule() {
    type OfficialBootstrap = typeof import('./storage/sync/officialAccountBootstrap')
    type Result = Awaited<ReturnType<OfficialBootstrap['initializeOfficialAccountBootstrap']>>
    return {
        initializeOfficialAccountBootstrap: vi.fn(async () => stub<Result>({ officialEnabled: false })), publishOfficialRevisionIfChanged: vi.fn(),
    } satisfies Partial<OfficialBootstrap>
}

export function officialAssetLedgerModule() {
    return {
        createAccountScopedOfficialAssetLedger: () => stub<ReturnType<typeof import('./storage/sync/officialAssetLedger')['createAccountScopedOfficialAssetLedger']>>({ reset: vi.fn() }),
        createNativeOfficialAssetLedger: async () => stub<Awaited<ReturnType<typeof import('./storage/sync/officialAssetLedger')['createNativeOfficialAssetLedger']>>>({ reset: vi.fn(), flush: vi.fn(async () => {}), clearAll: vi.fn(async () => {}) }),
    } satisfies Partial<typeof import('./storage/sync/officialAssetLedger')>
}

export function accountAssetAccessModule() {
    return {
        configureOfficialAccountAssetReader: vi.fn(), createStructuredAccountAssetReader: vi.fn(),
    } satisfies Partial<typeof import('./storage/accountAssetAccess')>
}

export function nativeOfficialAccountFlowModule() {
    type Flow = typeof import('./storage/sync/nativeOfficialAccountFlow')
    return {
        configureNativeOfficialAccountFlow: vi.fn(),
        restoreNativeOfficialAccountCandidate: vi.fn<Flow['restoreNativeOfficialAccountCandidate']>(),
        createNativeOfficialAccountFlowService: () => stub<ReturnType<Flow['createNativeOfficialAccountFlowService']>>({ flow: {}, snapshotRequestReauthentication: {} }),
        nativeOfficialAccountKeys: stub<Flow['nativeOfficialAccountKeys']>({ association: 'association', assetLedger: 'asset-ledger' }),
        normalizeNativeOfficialAccountCredential: (value: unknown) => value as ReturnType<Flow['normalizeNativeOfficialAccountCredential']>,
    } satisfies Partial<Flow>
}

export function nativeOfficialPublicationJobModule() {
    return {
        createNativeOfficialPublicationJobPublisher: vi.fn(() => stub<ReturnType<typeof import('./storage/sync/nativeOfficialPublicationJob')['createNativeOfficialPublicationJobPublisher']>>({})),
        createNativeOfficialAssetPublisher: vi.fn(() => stub<ReturnType<typeof import('./storage/sync/nativeOfficialPublicationJob')['createNativeOfficialAssetPublisher']>>({})),
    } satisfies Partial<typeof import('./storage/sync/nativeOfficialPublicationJob')>
}

export function nativeOfficialPublicationRecoveryModule(startup: BootstrapTestState) {
    type Recovery = typeof import('./storage/sync/nativeOfficialPublicationRecovery')
    return {
        createNativeOfficialPublicationRecovery: () => stub<ReturnType<Recovery['createNativeOfficialPublicationRecovery']>>({
            reconcile: startup.reconcile, reconcileSettled: startup.reconcile,
        }),
    } satisfies Partial<Recovery>
}

export function platformBlobStoreModule() {
    return { resolveBlobStore: vi.fn() } satisfies Partial<typeof import('./storage/platformBlobStore')>
}

export function syncConflictBackupModule() {
    return { getSyncConflictBackupStore: vi.fn() } satisfies Partial<typeof import('./storage/sync/syncConflictBackup')>
}

export function syncConflictSummaryModule() {
    return { formatNameList: vi.fn(), summarizePinnedSyncConflict: vi.fn() } satisfies Partial<typeof import('./storage/sync/syncConflictSummary')>
}

export function persistentRecordIteratorModule() {
    return { withPersistentRevisionLease: vi.fn() } satisfies Partial<typeof import('./storage/persistentRecordIterator')>
}

export function coldStorageModule() {
    return {
        getAccountColdStorageItem: vi.fn(),
    } satisfies Partial<typeof import('./process/coldstorage.svelte')>
}

export function storesModule() {
    type Stores = typeof import('./stores.svelte')
    return {
        MobileGUI: stub<Stores['MobileGUI']>({ set: vi.fn() }), botMakerMode: stub<Stores['botMakerMode']>({ set: vi.fn() }),
        selectedCharID: stub<Stores['selectedCharID']>({ set: vi.fn() }), loadedStore: stub<Stores['loadedStore']>({ set: vi.fn() }),
        DBState: stub<Stores['DBState']>({}), LoadingStatusState: stub<Stores['LoadingStatusState']>({ text: '' }),
        bootFailure: stub<Stores['bootFailure']>({ set: vi.fn() }),
    } satisfies Partial<Stores>
}

export function alertModule() {
    return {
        alertConfirm: vi.fn(), alertError: vi.fn(), alertInput: vi.fn(), alertLogin: vi.fn(), alertMd: vi.fn(),
        alertNormal: vi.fn(), alertSelect: vi.fn(), alertTOS: vi.fn(), alertRisuServiceTOS: vi.fn(), waitAlert: vi.fn(),
        alertToast: vi.fn(),
    } satisfies Partial<typeof import('./alert')>
}

export function characterCardsModule() {
    return { applyHubSelection: vi.fn(), characterURLImport: vi.fn(), downloadRisuHub: vi.fn(), hubURL: 'https://hub.invalid' } satisfies Partial<typeof import('./characterCards')>
}

export function androidSafBridgeModule() {
    return { isAndroidSafFileJobsEnabled: vi.fn(() => false) } satisfies Partial<typeof import('./storage/androidSafBridge')>
}

export function lifecycleCommitModule() {
    return { registerLifecycleCommitListeners: vi.fn() } satisfies Partial<typeof import('./storage/lifecycleCommit')>
}

export function deviceMarkersModule(startup: BootstrapTestState) {
    return { initializeDeviceMarkers: vi.fn(), reloadDeviceMarkers: vi.fn(), getDeviceMarkers: () => startup.markers } satisfies Partial<typeof import('./storage/deviceMarkers')>
}

export function bootAttemptModule(startup: BootstrapTestState) {
    return { markBootStage: startup.stages, markBootSuspect: vi.fn() } satisfies Partial<typeof import('./storage/bootAttempt')>
}

export function yieldToUiModule() {
    return { yieldToUi: async () => {} } satisfies Partial<typeof import('./ui/yieldToUi')>
}

export function nativeLocalUrlsModule() {
    return { initializeNativeLocalUrls: vi.fn() } satisfies Partial<typeof import('./nativeLocalUrls')>
}

export function iosNativeModule() {
    return { initializeIOSNative: vi.fn(), installIOSPersistenceLifecycle: vi.fn() } satisfies Partial<typeof import('./iosNative')>
}

export function externalStorageProductionModule(startup: BootstrapTestState) {
    return {
        installExternalStorageProduction: startup.external, installExternalSyncTransports: startup.transports,
        getExternalStorageSyncExitDrainAdapter: startup.exitDrain,
    } satisfies Partial<typeof import('./storage/sync/external/production')>
}

export function externalLwwProductionModule(startup: BootstrapTestState) {
    return { isExternalLwwRunning: () => startup.lwwRunning } satisfies Partial<typeof import('./storage/sync/external/lwwProduction')>
}

export function syncExitCoordinatorModule() {
    type Coordinator = typeof import('./storage/syncExitCoordinator')
    return { createSyncExitCoordinator: vi.fn(() => stub<ReturnType<Coordinator['createSyncExitCoordinator']>>({})) } satisfies Partial<Coordinator>
}

export function syncExitProductionModule() {
    return { configureSyncExitCoordinator: vi.fn(), registerWindowCloseDrain: vi.fn() } satisfies Partial<typeof import('./storage/syncExitProduction')>
}

export function serverSyncProductionModule(startup: BootstrapTestState) {
    type ServerSync = typeof import('./storage/sync/serverSyncProduction')
    type Controller = ReturnType<ServerSync['getServerSyncController']>
    return {
        createServerSyncExitDrainAdapter: startup.serverDrain, holdServerSyncAfterRestore: vi.fn(), initializeNativeSyncBindings: vi.fn(),
        getServerSyncController: () => stub<Controller>({ snapshot: () => stub<ReturnType<Controller['snapshot']>>({ paused: startup.serverPaused }) }),
        installServerSyncProduction: vi.fn(), disposeNativeSyncBindings: vi.fn(),
    } satisfies Partial<ServerSync>
}

export function externalStorageBridgeModule() {
    return { getExternalStorageBridge: vi.fn() } satisfies Partial<typeof import('./storage/sync/external/bridge')>
}

export function transformersModule() {
    return { releaseIdleTransformerModels: vi.fn() } satisfies Partial<typeof import('./process/transformers')>
}

export function inlayProviderImageModule() {
    return { forgetInlayProviderImages: vi.fn() } satisfies Partial<typeof import('./process/files/inlayProviderImage')>
}
