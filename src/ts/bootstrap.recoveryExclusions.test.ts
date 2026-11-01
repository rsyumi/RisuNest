import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import type { BootstrapTestState } from './bootstrap.testSupport'

const startupMocks = vi.hoisted(() => ({ checkNativeStartupStatus: vi.fn() }))
const defaultPlugins = vi.hoisted(() => ({ startDefaultPluginBootstrap: vi.fn() }))
vi.mock('./plugins/defaultPluginBootstrap.svelte', () => defaultPlugins)
const startup = vi.hoisted((): BootstrapTestState => ({
    native: true, exclusions: [], database: {} as any, reachUi: false,
    takeRendererRecovery: vi.fn(async () => false),
    credential: { id: 'synthetic-account', token: 'synthetic-token' },
    readVault: vi.fn(), reconcile: vi.fn(), external: vi.fn(), transports: vi.fn(), stages: vi.fn(),
    serverPaused: true, lwwRunning: false, exitDrain: vi.fn(), serverDrain: vi.fn(),
    stop: new Error('synthetic bootstrap test reached UI'),
    markers: { getItem: vi.fn(() => null), setItem: vi.fn(), removeItem: vi.fn(), flush: vi.fn(async () => {}) },
}))

vi.mock('svelte/store', async (importOriginal) => ({
    ...await importOriginal<typeof import('svelte/store')>(),
    get: vi.fn(() => false),
}))
vi.mock('@tauri-apps/plugin-fs', async () => (await import('./bootstrap.testSupport')).tauriFsModule())
vi.mock('@tauri-apps/api/webviewWindow', async () => (await import('./bootstrap.testSupport')).webviewWindowModule())
vi.mock('@tauri-apps/api/core', async () => (await import('./bootstrap.testSupport')).tauriCoreModule())
vi.mock('@tauri-apps/api/path', async () => (await import('./bootstrap.testSupport')).tauriPathModule())
vi.mock('./util', async () => (await import('./bootstrap.testSupport')).utilModule())
vi.mock('./gui/animation', async () => (await import('./bootstrap.testSupport')).animationModule())
vi.mock('./gui/colorscheme', async () => (await import('./bootstrap.testSupport')).colorschemeModule(startup))
vi.mock('./observer.svelte', async () => (await import('./bootstrap.testSupport')).observerModule())
vi.mock('./gui/guisize', async () => (await import('./bootstrap.testSupport')).guisizeModule())
vi.mock('./hotkey', async () => (await import('./bootstrap.testSupport')).hotkeyModule())
vi.mock('./process/modules', async () => (await import('./bootstrap.testSupport')).modulesModule())
vi.mock('./drive/accounter', async () => (await import('./bootstrap.testSupport')).accounterModule())
vi.mock('./storage/remoteSaveCleanup', async () => (await import('./bootstrap.testSupport')).remoteSaveCleanupModule())
vi.mock('./model/modellist', async () => (await import('./bootstrap.testSupport')).modellistModule())
vi.mock('./nativeScreenshotArchiveWriter', async () => (await import('./bootstrap.testSupport')).nativeScreenshotArchiveWriterModule())
vi.mock('./storage/nativePersistentMaintenance', async () => (await import('./bootstrap.testSupport')).nativePersistentMaintenanceModule())
vi.mock('./storage/nativeFileJobs', async () => (await import('./bootstrap.testSupport')).nativeFileJobsModule())
vi.mock('./storage/androidRisuSaveRouteProduction.svelte', async () => (await import('./bootstrap.testSupport')).androidRisuSaveRouteModule())
vi.mock('./storage/iosFiles', async () => (await import('./bootstrap.testSupport')).iosFilesModule())
vi.mock('./platform', async () => (await import('./platform.testSupport')).platformModule(() => (
    // bootstrap.ts reads only isTauri, isTauriAndroid and isTauriDesktop; iOS is the native shape that leaves the
    // desktop window and Android lifecycle paths out of these tests.
    startup.native ? { runtime: 'tauri', os: 'ios' } : { runtime: 'web', os: 'windows' })))
vi.mock('./storage/deviceSettings', async () => (await import('./bootstrap.testSupport')).deviceSettingsModule(startup))
vi.mock('./storage/persistentStorageRuntime', async () => (await import('./bootstrap.testSupport')).persistentStorageRuntimeModule())
vi.mock('./storage/nativeFileJobRecovery', async original => ({
    ...(await import('./bootstrap.testSupport')).nativeFileJobRecoveryModule(),
    acknowledgeRecoveredNativeRestores: (await original<typeof import('./storage/nativeFileJobRecovery')>()).acknowledgeRecoveredNativeRestores,
}))
vi.mock('./storage/persistentBootstrap', async () => (await import('./bootstrap.testSupport')).persistentBootstrapModule(startup))
vi.mock('./storage/database.svelte', async () => (await import('./bootstrap.testSupport')).databaseModule(startup))
vi.mock('./storage/databasePreparation', async () => (await import('./bootstrap.testSupport')).databasePreparationModule())
vi.mock('./storage/workingSetCatalog', async () => (await import('./bootstrap.testSupport')).workingSetCatalogModule())
vi.mock('./storage/workingSetResidency', async () => (await import('./bootstrap.testSupport')).workingSetResidencyModule())
vi.mock('./storage/persistentDataRuntime.svelte', async () => (await import('./bootstrap.testSupport')).persistentDataRuntimeModule())
vi.mock('./plugins/plugins.svelte', async () => (await import('./bootstrap.testSupport')).pluginsModule())
vi.mock('./storage/accountStorage', async () => (await import('./bootstrap.testSupport')).accountStorageModule())
vi.mock('./storage/nativeAccountCredential', async () => (await import('./bootstrap.testSupport')).nativeAccountCredentialModule(startup))
vi.mock('./storage/nativeDeviceSettings', async () => (await import('./bootstrap.testSupport')).nativeDeviceSettingsModule())
vi.mock('./storage/sync/officialAccountSnapshot', async () => (await import('./bootstrap.testSupport')).officialAccountSnapshotModule())
vi.mock('./storage/sync/officialAccountBootstrap', async () => (await import('./bootstrap.testSupport')).officialAccountBootstrapModule())
vi.mock('./storage/sync/officialAssetLedger', async () => (await import('./bootstrap.testSupport')).officialAssetLedgerModule())
vi.mock('./storage/accountAssetAccess', async () => (await import('./bootstrap.testSupport')).accountAssetAccessModule())
vi.mock('./storage/sync/nativeOfficialAccountFlow', async () => (await import('./bootstrap.testSupport')).nativeOfficialAccountFlowModule())
vi.mock('./storage/sync/nativeOfficialPublicationJob', async () => (await import('./bootstrap.testSupport')).nativeOfficialPublicationJobModule())
vi.mock('./storage/sync/nativeOfficialPublicationRecovery', async () => (await import('./bootstrap.testSupport')).nativeOfficialPublicationRecoveryModule(startup))
vi.mock('./storage/platformBlobStore', async () => (await import('./bootstrap.testSupport')).platformBlobStoreModule())
vi.mock('./storage/sync/syncConflictBackup', async () => (await import('./bootstrap.testSupport')).syncConflictBackupModule())
vi.mock('./storage/sync/syncConflictSummary', async () => (await import('./bootstrap.testSupport')).syncConflictSummaryModule())
vi.mock('./storage/persistentRecordIterator', async () => (await import('./bootstrap.testSupport')).persistentRecordIteratorModule())
vi.mock('./process/coldstorage.svelte', async () => (await import('./bootstrap.testSupport')).coldStorageModule())
vi.mock('./globalApi.svelte', () => ({
    forageStorage: { Init: vi.fn(async () => {}), isAccount: false, setAccountModeForSession: vi.fn() }, saveDb: vi.fn(),
    getUncleanables: vi.fn(), getBasename: vi.fn(), invalidateAssetSourceCache: vi.fn(), setUsingSw: vi.fn(),
}))
vi.mock('./stores.svelte', async () => (await import('./bootstrap.testSupport')).storesModule())
vi.mock('./alert', async () => (await import('./bootstrap.testSupport')).alertModule())
vi.mock('./characterCards', async () => (await import('./bootstrap.testSupport')).characterCardsModule())
vi.mock('./storage/androidSafBridge', async () => (await import('./bootstrap.testSupport')).androidSafBridgeModule())
vi.mock('./storage/lifecycleCommit', async () => (await import('./bootstrap.testSupport')).lifecycleCommitModule())
vi.mock('./nativeStartup', () => startupMocks)


vi.mock('./storage/deviceMarkers', async () => (await import('./bootstrap.testSupport')).deviceMarkersModule(startup))
vi.mock('./storage/recoveryMode.svelte', async (importOriginal) => ({
    ...await importOriginal<typeof import('./storage/recoveryMode.svelte')>(),
    finishBoot: vi.fn(async () => {}),
    takeRendererRecovery: startup.takeRendererRecovery,
}))
vi.mock('./storage/bootAttempt', async () => (await import('./bootstrap.testSupport')).bootAttemptModule(startup))
vi.mock('./ui/yieldToUi', async () => (await import('./bootstrap.testSupport')).yieldToUiModule())
vi.mock('./nativeLocalUrls', async () => (await import('./bootstrap.testSupport')).nativeLocalUrlsModule())
vi.mock('./iosNative', async () => (await import('./bootstrap.testSupport')).iosNativeModule())
vi.mock('./storage/sync/external/production', async () => (await import('./bootstrap.testSupport')).externalStorageProductionModule(startup))
vi.mock('./storage/sync/external/lwwProduction', async () => (await import('./bootstrap.testSupport')).externalLwwProductionModule(startup))
vi.mock('./storage/syncExitCoordinator', async () => (await import('./bootstrap.testSupport')).syncExitCoordinatorModule())
vi.mock('./storage/syncExitProduction', async () => (await import('./bootstrap.testSupport')).syncExitProductionModule())
vi.mock('./storage/sync/serverSyncProduction', async () => (await import('./bootstrap.testSupport')).serverSyncProductionModule(startup))
vi.mock('./storage/sync/external/bridge', async () => (await import('./bootstrap.testSupport')).externalStorageBridgeModule())
vi.mock('./process/transformers', async () => (await import('./bootstrap.testSupport')).transformersModule())
vi.mock('./process/files/inlayProviderImage', async () => (await import('./bootstrap.testSupport')).inlayProviderImageModule())

import { loadData } from './bootstrap'
import { changeLanguage, language } from 'src/lang'
import { languageEnglish } from 'src/lang/en'
import { languageKorean } from 'src/lang/ko'
import { bootstrapPersistentDatabase } from './storage/persistentBootstrap'
import { safeStructuredClone } from './safeStructuredClone'
import { afterPluginsStarted, keepPluginsOffForThisStart, loadPlugins } from './plugins/plugins.svelte'
import { pluginStartupGate, configurePluginStartup, setPluginStartupSyncExcluded, retryPluginStartup } from './plugins/pluginStartupGate'
import { shouldReconcileNativeFileJobs, reconcileNativeFileJobsBeforeBootstrap } from './storage/nativeFileJobRecovery'
import { alertError, alertNormal, alertToast, alertTOS, waitAlert } from './alert'
import { invoke } from '@tauri-apps/api/core'
import { initializePersistentStorage } from './storage/persistentStorageRuntime'
import { bootFailure, LoadingStatusState } from './stores.svelte'
import { loadRisuAccountData } from './drive/accounter'
import { initializeOfficialAccountBootstrap } from './storage/sync/officialAccountBootstrap'
import { initializeNativeSyncBindings, installServerSyncProduction } from './storage/sync/serverSyncProduction'
import { createSyncExitCoordinator } from './storage/syncExitCoordinator'
import { getExternalStorageBridge } from './storage/sync/external/bridge'
import { beginDisplayWork, installDisplayRecovery, isDisplayRecoveryMarked } from './displayRecovery.svelte'

beforeEach(() => {
    vi.clearAllMocks()
    vi.mocked(shouldReconcileNativeFileJobs).mockReturnValue(false)
    vi.mocked(reconcileNativeFileJobsBeforeBootstrap).mockResolvedValue({
        pendingRestoreAcknowledgements: [], pendingOfficialPublications: [], interruptedRestores: [],
        libraryRestoreCommitted: false, pendingSnapshotBodies: [],
    })
    startup.reachUi = false
    vi.stubGlobal('localStorage', { getItem: () => null, setItem: vi.fn(), removeItem: vi.fn() })
    vi.stubGlobal('navigator', {})
    startup.database = { characters: [], botPresets: [], account: { id: 'web-account', token: 'synthetic-web-token' } }
    startup.readVault.mockResolvedValue(startup.credential)
    vi.spyOn(console, 'error').mockImplementation(() => undefined)
})

afterEach(() => {
    setPluginStartupSyncExcluded(false)
    configurePluginStartup(() => false, async () => {})
    vi.restoreAllMocks()
    vi.unstubAllGlobals()
})

describe('bootstrap recovery exclusions', () => {
    it.each([false, true])('bypasses excluded sync before plugin startup, preserving plugin exclusion and restore acknowledgement (plugins excluded=%s)', async pluginsExcluded => {
        startup.native = true
        startup.exclusions = pluginsExcluded ? ['sync', 'plugins'] : ['sync']
        vi.mocked(shouldReconcileNativeFileJobs).mockReturnValue(true)
        vi.mocked(reconcileNativeFileJobsBeforeBootstrap).mockResolvedValue({
            pendingRestoreAcknowledgements: ['synthetic-restored-job'], pendingOfficialPublications: [], interruptedRestores: [],
            libraryRestoreCommitted: true, pendingSnapshotBodies: [],
        })
        const listeners = new Set<() => void | Promise<void>>()
        vi.mocked(afterPluginsStarted).mockImplementation(listener => { listeners.add(listener); return () => {listeners.delete(listener)} })
        let started = 0
        const load = async () => {
            const before = vi.mocked(invoke).mock.calls.length
            const ready = await pluginStartupGate.ensure()
            expect(vi.mocked(invoke).mock.calls.slice(before).map(([command]) => command)).not.toContain('pds_lww_binding_state')
            if (!ready) return
            started++
            for (const listener of listeners) await listener()
        }
        configurePluginStartup(() => true, load)
        vi.mocked(loadPlugins).mockImplementationOnce(load)
        await loadData()
        expect(started).toBe(pluginsExcluded ? 0 : 1)
        expect(invoke).toHaveBeenCalledWith('native_file_job_forget', {jobId:'synthetic-restored-job'})
        if (!pluginsExcluded) {
            const beforeRetry = vi.mocked(invoke).mock.calls.length
            await retryPluginStartup()
            expect(started).toBe(2)
            expect(vi.mocked(invoke).mock.calls.slice(beforeRetry).map(([command]) => command)).not.toContain('pds_lww_binding_state')
            expect(installServerSyncProduction).toHaveBeenCalledExactlyOnceWith({resumeBound:false})
        }
    })
    it.each([true, false])('acknowledges recovered restores after the plugin decision (excluded=%s)', async excluded => {
        startup.native = true
        startup.exclusions = excluded ? ['plugins'] : []
        vi.mocked(shouldReconcileNativeFileJobs).mockReturnValue(true)
        const recovered = { pendingRestoreAcknowledgements: ['synthetic-restored-job'], pendingOfficialPublications: [], interruptedRestores: [],
            libraryRestoreCommitted: true, pendingSnapshotBodies: [] }
        vi.mocked(reconcileNativeFileJobsBeforeBootstrap).mockResolvedValue(recovered)
        const listeners = new Set<() => void | Promise<void>>()
        vi.mocked(afterPluginsStarted).mockImplementation(listener => {
            listeners.add(listener)
            return () => { listeners.delete(listener) }
        })
        await loadData()
        if (!excluded) {
            expect(invoke).not.toHaveBeenCalledWith('native_file_job_forget', expect.anything())
            // Closing while receive is pending leaves the job for the next launch.
            listeners.clear()
            await loadData()
            expect(invoke).not.toHaveBeenCalledWith('native_file_job_forget', expect.anything())
            expect(listeners.size).toBe(1)
            for (const started of listeners) await started()
        }
        expect(invoke).toHaveBeenCalledWith('native_file_job_forget', { jobId: 'synthetic-restored-job' })
        expect(listeners.size).toBe(0)
        if (excluded) expect(loadPlugins).not.toHaveBeenCalled()
    })
    it.each([true, false])('skips account and sync startup work while reaching UI (native=%s)', async (native) => {
        startup.native = native
        startup.exclusions = ['account', 'sync']
        await loadData()
        expect(bootFailure.set).toHaveBeenLastCalledWith(expect.objectContaining({
            stage: 'ui-state', message: startup.stop.message,
        }))
        expect(initializeOfficialAccountBootstrap).not.toHaveBeenCalled()
        expect(loadRisuAccountData).not.toHaveBeenCalled()
        expect(startup.reconcile).not.toHaveBeenCalled()
        expect(startup.external).not.toHaveBeenCalled()
        expect(startup.stages.mock.calls.flat()).not.toContain('account-bootstrap')
        expect(startup.stages.mock.calls.flat()).not.toContain('service-worker')
        if (native) {
            expect(initializeNativeSyncBindings).toHaveBeenCalledOnce()
            // The sync switches in settings need the transports even while sync stays stopped.
            expect(installServerSyncProduction).toHaveBeenCalledExactlyOnceWith({ resumeBound: false })
            expect(startup.transports).toHaveBeenCalledOnce()
            expect(startup.stages.mock.calls.flat()).toContain('drive-sync')
            expect(startup.readVault).toHaveBeenCalledOnce()
            expect(startup.database.account).toBe(startup.credential)
            expect(invoke).toHaveBeenCalledWith('native_log_set_file_enabled', { enabled: false })
        } else {
            expect(invoke).not.toHaveBeenCalledWith('native_log_set_file_enabled', expect.anything())
            expect(initializeNativeSyncBindings).not.toHaveBeenCalled()
            expect(installServerSyncProduction).not.toHaveBeenCalled()
            expect(startup.transports).not.toHaveBeenCalled()
            expect(startup.stages.mock.calls.flat()).not.toContain('drive-sync')
        }
    })

    it.each([true, false])('keeps plugins off for the whole run when they are excluded (native=%s)', async (native) => {
        startup.native = native
        startup.exclusions = ['plugins']
        await loadData()
        expect(bootFailure.set).toHaveBeenLastCalledWith(expect.objectContaining({ stage: 'ui-state' }))
        expect(keepPluginsOffForThisStart).toHaveBeenCalledOnce()
        expect(loadPlugins).not.toHaveBeenCalled()
    })

    it('leaves plugins on when they are not excluded', async () => {
        startup.native = true
        startup.exclusions = []
        await loadData()
        expect(keepPluginsOffForThisStart).not.toHaveBeenCalled()
        expect(loadPlugins).toHaveBeenCalledOnce()
    })

    it('reaches UI when registering the sync transports fails while sync is left off', async () => {
        startup.native = true
        startup.exclusions = ['sync']
        vi.mocked(installServerSyncProduction).mockRejectedValueOnce(new Error('synthetic transport failure'))
        await loadData()
        expect(bootFailure.set).toHaveBeenLastCalledWith(expect.objectContaining({
            stage: 'ui-state', message: startup.stop.message,
        }))
        expect(startup.external).not.toHaveBeenCalled()
        expect(startup.transports).toHaveBeenCalledOnce()
    })

    it.each([true, false])('runs account and sync startup work when enabled (native=%s)', async (native) => {
        startup.native = native
        startup.exclusions = []
        await loadData()
        expect(bootFailure.set).toHaveBeenLastCalledWith(expect.objectContaining({
            stage: 'ui-state', message: startup.stop.message,
        }))
        expect(initializeOfficialAccountBootstrap).toHaveBeenCalledOnce()
        expect(startup.stages.mock.calls.flat()).toContain('account-bootstrap')
        if (native) {
            expect(initializeNativeSyncBindings).toHaveBeenCalledOnce()
            expect(installServerSyncProduction).toHaveBeenCalledOnce()
            expect(startup.reconcile).toHaveBeenCalledOnce()
            expect(startup.external).toHaveBeenCalledOnce()
            expect(startup.transports).not.toHaveBeenCalled()
            expect(startup.stages.mock.calls.flat()).toContain('drive-sync')
        } else {
            expect(initializeNativeSyncBindings).not.toHaveBeenCalled()
            expect(installServerSyncProduction).not.toHaveBeenCalled()
            expect(loadRisuAccountData).toHaveBeenCalledOnce()
            expect(startup.stages.mock.calls.flat()).toContain('service-worker')
        }
    })

    it('attributes a fatal server sync startup failure to the sync stage', async () => {
        startup.native = true
        startup.exclusions = []
        const failure = new Error('synthetic sync startup failure')
        vi.mocked(installServerSyncProduction).mockRejectedValueOnce(failure)
        await loadData()
        expect(bootFailure.set).toHaveBeenLastCalledWith(expect.objectContaining({
            stage: 'drive-sync', message: failure.message,
        }))
        expect(startup.external).not.toHaveBeenCalled()
    })
})

describe('exit sync after a start that left sync off', () => {
    type ExitDependencies = Parameters<typeof createSyncExitCoordinator>[0]
    const selection = (kind: 'server' | 'external', connectionId: string) => ({ kind, connectionId, selectionEpoch: '2', paused: false })
    async function exitTarget(current: ReturnType<typeof selection>, clearedDuringSession = false) {
        startup.native = true
        startup.exclusions = ['sync']
        startup.exitDrain.mockImplementation(() => ({ id: `external:${current.connectionId}:2` }))
        startup.serverDrain.mockImplementation((id: string) => ({ id }))
        vi.mocked(getExternalStorageBridge).mockReturnValue({
            captureExitTarget: async () => ({ revision: '5', libraryEpoch: '1', selection: current }),
        } as never)
        await loadData()
        const dependencies = vi.mocked(createSyncExitCoordinator).mock.calls.at(-1)![0] as ExitDependencies
        if (clearedDuringSession) startup.exclusions = []
        await dependencies.acquireEditFence()
        const target = await dependencies.captureTarget()
        return { target, drain: await dependencies.selectedDrain() }
    }
    beforeEach(() => {
        startup.serverPaused = true
        startup.lwwRunning = false
    })

    it('drains an external target the user started during the session without starting automatic backups', async () => {
        startup.lwwRunning = true
        const { target, drain } = await exitTarget(selection('external', 'connection-1'))
        expect(target.selectionId).toBe('external:connection-1:2')
        expect(drain?.id).toBe('external:connection-1:2')
        expect(startup.external).not.toHaveBeenCalled()
        expect(startup.transports).toHaveBeenCalled()
    })

    it('leaves an external target alone when it was never started', async () => {
        const { target, drain } = await exitTarget(selection('external', 'connection-1'))
        expect(target.selectionId).toBe('none:2')
        expect(drain).toBeNull()
        expect(startup.exitDrain).not.toHaveBeenCalled()
    })

    it('drains a server target the user started during the session', async () => {
        startup.serverPaused = false
        const { target, drain } = await exitTarget(selection('server', 'server-1'))
        expect(target.selectionId).toBe('server:server-1:2')
        expect(drain?.id).toBe('server:server-1:2')
    })

    it('leaves a server target alone when it was never started', async () => {
        const { target, drain } = await exitTarget(selection('server', 'server-1'))
        expect(target.selectionId).toBe('none:2')
        expect(drain).toBeNull()
    })

    it('does not start sync at exit when the exclusion was cleared during the session', async () => {
        const { target, drain } = await exitTarget(selection('external', 'connection-1'), true)
        expect(target.selectionId).toBe('none:2')
        expect(drain).toBeNull()
        expect(startup.external).not.toHaveBeenCalled()
    })
})

describe('renderer recovery notice', () => {
    beforeEach(() => {
        startup.native = true
        startup.exclusions = []
        startup.reachUi = true
        startup.reconcile.mockResolvedValue(undefined)
    })

    it('shows a toast after the terms answer and queued dialogs so neither can replace it', async () => {
        let answerTerms!: (accepted: boolean) => void
        vi.mocked(alertTOS).mockReturnValueOnce(new Promise((resolve) => { answerTerms = resolve }))
        startup.takeRendererRecovery.mockResolvedValueOnce(true)
        await loadData()
        expect(bootFailure.set).toHaveBeenLastCalledWith(null)
        expect(startup.takeRendererRecovery).not.toHaveBeenCalled()
        expect(defaultPlugins.startDefaultPluginBootstrap).not.toHaveBeenCalled()
        answerTerms(true)
        await vi.waitFor(() => expect(alertToast).toHaveBeenCalledExactlyOnceWith(language.risuNest.startup.rendererRecovered))
        await vi.waitFor(() => expect(defaultPlugins.startDefaultPluginBootstrap).toHaveBeenCalledOnce())
        expect(alertNormal).not.toHaveBeenCalled()
        expect(vi.mocked(waitAlert).mock.invocationCallOrder.at(-1))
            .toBeLessThan(vi.mocked(alertToast).mock.invocationCallOrder[0])
    })

    it('leaves the answer untaken when the terms are declined', async () => {
        const reload = vi.fn()
        vi.stubGlobal('location', { reload })
        vi.mocked(alertTOS).mockResolvedValueOnce(false)
        await loadData()
        await vi.waitFor(() => expect(reload).toHaveBeenCalledOnce())
        expect(startup.takeRendererRecovery).not.toHaveBeenCalled()
        expect(alertToast).not.toHaveBeenCalled()
        expect(defaultPlugins.startDefaultPluginBootstrap).not.toHaveBeenCalled()
    })

    it('does not arm automatic plugin installation when startup excludes plugins', async () => {
        startup.exclusions = ['plugins']
        let answerTerms!: (accepted: boolean) => void
        vi.mocked(alertTOS).mockReturnValueOnce(new Promise(resolve => { answerTerms = resolve }))
        await loadData()
        startup.exclusions = []
        answerTerms(true)
        await vi.waitFor(() => expect(startup.takeRendererRecovery).toHaveBeenCalled())
        expect(defaultPlugins.startDefaultPluginBootstrap).not.toHaveBeenCalled()
    })
})

describe('display recovery record', () => {
    beforeEach(() => {
        startup.native = true
        startup.exclusions = []
        startup.reachUi = true
        startup.reconcile.mockResolvedValue(undefined)
    })

    it('marks the chats the last run left in the display record and drops marks of characters gone after trash expiry', async () => {
        const kept = { characterId: 'kept-character', conversationId: 'kept-chat' }
        const gone = { characterId: 'gone-character', conversationId: 'gone-chat' }
        startup.markers.getItem.mockImplementation((key) => key === 'risuNestDisplayRecovery'
            ? JSON.stringify({ working: [kept, gone], marked: [] })
            : null)
        startup.database.characters = [{ chaId: kept.characterId }]
        vi.mocked(alertTOS).mockReturnValueOnce(new Promise(() => {}))
        try {
            await loadData()
            expect(startup.markers.setItem).toHaveBeenCalledWith('risuNestDisplayRecovery', JSON.stringify({ working: [], marked: [kept, gone] }))
            expect(isDisplayRecoveryMarked(kept)).toBe(true)
            await vi.waitFor(() => expect(isDisplayRecoveryMarked(gone)).toBe(false))
            expect(isDisplayRecoveryMarked(kept)).toBe(true)
        } finally {
            startup.markers.getItem.mockImplementation(() => null)
            await installDisplayRecovery(null)
        }
    })

    it('removes the record of display work still running when the app exits normally', async () => {
        type ExitDependencies = Parameters<typeof createSyncExitCoordinator>[0]
        vi.mocked(alertTOS).mockReturnValueOnce(new Promise(() => {}))
        try {
            await loadData()
            const target = { characterId: 'streaming-character', conversationId: 'streaming-chat' }
            await beginDisplayWork(target).ready
            expect(startup.markers.setItem).toHaveBeenLastCalledWith('risuNestDisplayRecovery', JSON.stringify({ working: [target], marked: [] }))
            const dependencies = vi.mocked(createSyncExitCoordinator).mock.calls.at(-1)![0] as ExitDependencies
            await dependencies.prepareExit!()
            expect(startup.markers.removeItem).toHaveBeenLastCalledWith('risuNestDisplayRecovery')
        } finally {
            await installDisplayRecovery(null)
        }
    })
})

describe('native setup failure', () => {
    it('stops bootstrap before diagnostics and exposes the existing failure panel state', async () => {
        startup.native = true
        startup.exclusions = []
        const failure = new Error('synthetic native setup failure')
        startupMocks.checkNativeStartupStatus.mockRejectedValueOnce(failure)

        await loadData()

        expect(invoke).not.toHaveBeenCalled()
        expect(initializePersistentStorage).not.toHaveBeenCalled()
        expect(loadPlugins).not.toHaveBeenCalled()
        expect(initializeNativeSyncBindings).not.toHaveBeenCalled()
        expect(installServerSyncProduction).not.toHaveBeenCalled()
        expect(bootFailure.set).toHaveBeenLastCalledWith({
            kind: 'unknown',
            message: failure.message,
            stage: 'native-setup',
        })
        expect(alertError).not.toHaveBeenCalled()
    })
})

describe('startup language', () => {
    // The suite unstubs every global after each test, the setup's clone that changeLanguage uses included.
    beforeEach(() => vi.stubGlobal('safeStructuredClone', safeStructuredClone))
    afterEach(() => changeLanguage('en'))

    // The entry applies the device language before the app mounts; Korean stands in for it here.
    it.each([
        ['en', languageEnglish.risuNest.startup.data],
        [undefined, languageKorean.risuNest.startup.data],
    ])('keeps the start language until the library language (%s) is read', async (libraryLanguage, dataText) => {
        startup.native = true
        startup.exclusions = []
        const shown: string[] = []
        vi.mocked(initializePersistentStorage).mockImplementationOnce(async () => { shown.push(LoadingStatusState.text) })
        vi.mocked(bootstrapPersistentDatabase).mockImplementationOnce(async ({ onPhase }) => {
            await onPhase?.('storage')
            shown.push(LoadingStatusState.text)
            await onPhase?.('data', libraryLanguage)
            shown.push(LoadingStatusState.text)
            return { database: startup.database, revision: 1 }
        })
        changeLanguage('ko')

        await loadData()

        const opening = languageKorean.risuNest.startup.storage
        expect(shown).toEqual([opening, opening, dataText])
    })
})
