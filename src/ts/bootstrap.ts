import {
    writeFile,
    BaseDirectory,
    readFile,
    exists,
    mkdir,
    readDir,
    remove
} from "@tauri-apps/plugin-fs"
import { changeFullscreen, sleep } from "./util"
import { get } from "svelte/store";
import { setDatabase, getDatabase, type Database } from "./storage/database.svelte";
import { getDeviceSettings, loadDeviceSettings } from "./storage/deviceSettings";
import { setNativeLogFileEnabled } from "./nativeLog";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { MobileGUI, botMakerMode, selectedCharID, loadedStore, LoadingStatusState, bootFailure, type BootFailure } from "./stores.svelte";
import { loadPlugins, loadPluginsAfterAuthoritativeRestore } from "./plugins/plugins.svelte";
import { alertConfirm, alertError, alertInput, alertLogin, alertMd, alertNormal, alertSelect, alertTOS, waitAlert } from "./alert";
import { applyHubSelection, characterURLImport, downloadRisuHub, hubURL } from "./characterCards";
import { initializeNativeLocalUrls } from "./nativeLocalUrls";
import { loadRisuAccountData } from "./drive/accounter";
import { updateAnimationSpeed } from "./gui/animation";
import { updateColorScheme, updateTextThemeAndCSS } from "./gui/colorscheme";
import { changeLanguage, language } from "src/lang";
import { startObserveDom } from "./observer.svelte";
import { updateGuisize } from "./gui/guisize";
import { initMobileGesture } from "./hotkey";
import { moduleUpdate } from "./process/modules";
import {
    AccountStorage,
    resetAccountStorageSession,
    type AccountStorageCache,
} from "./storage/accountStorage";
import { getAccountColdStorageItem } from "./process/coldstorage.svelte";
import { getRemoteSaveCleanupAction, getRemoteSavePayloadName } from "./storage/remoteSaveCleanup";
import {
    forageStorage,
    saveDb,
    getUncleanables,
    getBasename,
    invalidateAssetSourceCache,
    setUsingSw
} from "./globalApi.svelte";
import { isTauri, isTauriAndroid, isTauriDesktop } from "./platform";
import { registerModelDynamic } from "./model/modellist";
import {
    prepareDatabaseForPersistence,
    prepareDatabaseForBootstrap,
    preparePersistentRootForWorkingSet,
} from "./storage/databasePreparation";
import { bootstrapPersistentDatabase } from "./storage/persistentBootstrap";
import {
    createCatalogPresetWorkingSet,
    hasIncompletePersistentWorkingSet,
    isWorkingSetCharacterStub,
    isCatalogPresetWorkingSet,
    projectCatalogWorkingSet,
    projectCompleteScalableWorkingSet,
} from "./storage/workingSetCatalog";
import { workingSetResidency } from "./storage/workingSetResidency";
import {
    getPersistentDataRuntime,
    initializeActiveWorkingSet,
    configurePersistentDataRuntime,
    hasPendingOfficialPublication,
    publishCurrentOfficialRevision,
} from "./storage/persistentDataRuntime.svelte";
import { registerLifecycleCommitListeners } from "./storage/lifecycleCommit";
import { releaseIdleTransformerModels } from "./process/transformers";
import { forgetInlayProviderImages } from "./process/files/inlayProviderImage";
import { platform as nativePlatform } from '@tauri-apps/plugin-os'
import { resolveBlobStore } from "./storage/platformBlobStore";
import {
    OfficialAccountSnapshotAdapter,
    createOfficialAssociationMarkers,
} from "./storage/sync/officialAccountSnapshot";
import { getSyncConflictBackupStore } from "./storage/sync/syncConflictBackup";
import { formatNameList, summarizePinnedSyncConflict } from "./storage/sync/syncConflictSummary";
import { withPersistentRevisionLease } from "./storage/persistentRecordIterator";
import {
    activateNativeAssetRepository,
    initializePersistentStorage,
} from "./storage/persistentStorageRuntime";
import {
    acknowledgeRecoveredNativeRestores,
    reconcileNativeFileJobsBeforeBootstrap,
    shouldReconcileNativeFileJobs,
} from "./storage/nativeFileJobRecovery";
import { registerAndroidRisuSaveRoute } from "./storage/androidRisuSaveRouteProduction.svelte";
import { isAndroidSafFileJobsEnabled } from "./storage/androidSafBridge";
import {
    describeScreenshotPublicationError,
    listenRecoveredAndroidScreenshotPublications,
} from "./nativeScreenshotArchiveWriter";
import { initializeIOSNative, installIOSPersistenceLifecycle } from "./iosNative";
import { restartNativeApp, schedulePeriodicNativeSnapshot } from "./storage/nativePersistentMaintenance";
import { yieldToUi } from './ui/yieldToUi'
import { markBootStage, markBootSuspect } from './storage/bootAttempt'
import {
    finishBoot,
    isStartupExcluded,
    type RecoveryExclusion,
} from './storage/recoveryMode.svelte'
import {
    initializeOfficialAccountBootstrap,
} from "./storage/sync/officialAccountBootstrap";
import { createAccountScopedOfficialAssetLedger } from "./storage/sync/officialAssetLedger";
import {
    configureOfficialAccountAssetReader,
    createStructuredAccountAssetReader,
} from "./storage/accountAssetAccess";
import { createNativeAccountCredentialVault } from "./storage/nativeAccountCredential";
import {
    createNativeDeviceSettings,
    createNativeDeviceSettingsBag,
} from "./storage/nativeDeviceSettings";
import { getDeviceMarkers, initializeDeviceMarkers } from "./storage/deviceMarkers";
import {
    configureNativeOfficialAccountFlow,
    createNativeOfficialAccountFlowService,
    type NativeOfficialAccountFlow,
    type NativeOfficialAccountFlowService,
    nativeOfficialAccountKeys,
    normalizeNativeOfficialAccountCredential,
} from "./storage/sync/nativeOfficialAccountFlow";
import {
    NativeFileJobError,
    runNativeOfficialAccountSnapshotRestore,
} from "./storage/nativeFileJobs";
import { createNativeOfficialPublicationJobPublisher } from "./storage/sync/nativeOfficialPublicationJob";
import {
    createNativeOfficialPublicationRecovery,
    type NativeOfficialPublicationRecovery,
} from "./storage/sync/nativeOfficialPublicationRecovery";
import { checkNativeStartupStatus } from './nativeStartup'
import {
    createSyncExitCoordinator,
    type SyncExitDrainAdapter,
} from './storage/syncExitCoordinator'
import {
    configureSyncExitCoordinator,
    registerWindowCloseDrain,
} from './storage/syncExitProduction'
import { getExternalStorageBridge } from './storage/sync/external/bridge'
import { createServerSyncExitDrainAdapter } from './storage/sync/serverSyncProduction'

const appWindow = isTauri ? getCurrentWebviewWindow() : null
let disposeLifecycleCommitListeners: (() => void) | undefined
let disposeMacosLifecycle: (() => void) | undefined
let disposeWindowCloseDrain: (() => void) | undefined
let disposeAndroidScreenshotRecovery: (() => void) | undefined

function registerAndroidScreenshotPublicationRecovery() {
    if (
        disposeAndroidScreenshotRecovery
        || !isTauriAndroid
        || !isAndroidSafFileJobsEnabled()
    ) return
    disposeAndroidScreenshotRecovery = listenRecoveredAndroidScreenshotPublications((terminal) => {
        const partialDestinationMayRemain = terminal.warningCodes
            .includes('partial-destination-may-remain')
        if (terminal.state === 'cancelled' && !partialDestinationMayRemain) return
        if (terminal.state === 'succeeded') {
            alertNormal(language.screenshotSaved)
            return
        }
        const error = Object.assign(
            new Error(terminal.message ?? terminal.code ?? 'Android screenshot publication failed'),
            { warningCodes: terminal.warningCodes },
        )
        alertError(language.screenshotFailed.replace(
            '{error}',
            describeScreenshotPublicationError(
                error,
                language.screenshotPartialDestinationMayRemain,
            ),
        ))
    }, (error) => alertError(language.screenshotFailed.replace(
        '{error}',
        error instanceof Error ? error.message : String(error),
    )))
}

/**
 * Boot stages whose failures mean the local persistent store could not be
 * opened or bootstrapped. Both of them go through `store.open()`.
 */
const persistentStoreOpenStages = new Set([
    'persistent-storage',
    'device-settings',
    'persistent-database',
])

function describeBootFailureError(error: unknown): string {
    if (error instanceof Error) return error.message
    if (typeof error === 'string') return error
    if (error && typeof error === 'object') {
        const message = (error as { message?: unknown }).message
        if (typeof message === 'string') return message
    }
    return String(error)
}

/**
 * Classifies a startup failure so the recovery panel can explain what the user
 * has to do. Pure so it can be tested without booting the application.
 */
export function classifyBootFailure(error: unknown, stage?: string): BootFailure {
    const message = describeBootFailureError(error)
    if (message.includes('unsupported persistent schema version')) {
        return { kind: 'schema-unsupported', message, stage }
    }
    if (stage !== undefined && persistentStoreOpenStages.has(stage)) {
        return { kind: 'store-open', message, stage }
    }
    return { kind: 'unknown', message, stage }
}

/**
 * Loads the application data.
 */
export async function loadData() {
    if (get(loadedStore)) return
    let stage = 'startup'
    LoadingStatusState.startedAt = performance.now()
    bootFailure.set(null)
    const transition = async (nextStage: string, text: string) => {
        stage = nextStage
        markBootStage(nextStage)
        LoadingStatusState.text = text
        await yieldToUi()
    }
    const excluded = (name: RecoveryExclusion): boolean =>
        isStartupExcluded(name, getDeviceSettings().startupExclusions)
    LoadingStatusState.text = language.risuNest.startup.storage
    try {
        if (isTauri) {
            stage = 'native-setup'
            await checkNativeStartupStatus()
        }
        if (isTauri) {
            await transition('app-data-directories', language.risuNest.startup.storage)
            if (isTauriDesktop) appWindow.maximize()
            if (!await exists('', { baseDir: BaseDirectory.AppData })) {
                await mkdir('', { baseDir: BaseDirectory.AppData })
            }
            if (!await exists('assets', { baseDir: BaseDirectory.AppData })) {
                await mkdir('assets', { baseDir: BaseDirectory.AppData })
            }
        } else {
            stage = 'browser-storage'
            await forageStorage.Init()
        }

        await transition('persistent-storage', language.risuNest.startup.storage)
        await initializePersistentStorage()
        if (isTauri) {
            stage = 'device-settings'
            await initializeDeviceMarkers()
        }
        const markers = getDeviceMarkers()
        applyHubSelection(markers)
        const deviceSettings = loadDeviceSettings(markers)
        if (isTauri) {
            stage = 'native-log'
            try {
                await setNativeLogFileEnabled(deviceSettings.nativeFileLogEnabled)
            } catch (error) {
                console.error('Native file logging reconciliation failed', error)
            }
        }
        stage = 'native-file-jobs'
        const recoveredNativeFileJobs = isTauri
            ? await reconcileNativeFileJobsBeforeBootstrap(undefined, {
                reconcileRestores: shouldReconcileNativeFileJobs(
                    isTauriDesktop,
                    isTauriAndroid,
                    isAndroidSafFileJobsEnabled(),
                ),
            })
            : {
                pendingRestoreAcknowledgements: [],
                pendingOfficialPublications: [],
            }
        const recoveredNativeRestoreJobs =
            recoveredNativeFileJobs.pendingRestoreAcknowledgements
        const runtime = getPersistentDataRuntime()
        const flushLifecycle = (reason: string, locally = false): Promise<void> => {
            forgetInlayProviderImages()
            void releaseIdleTransformerModels().catch(() => {
                console.warn('Optional model memory cleanup failed')
            })
            return locally ? runtime.flushPendingDataLocally(reason) : runtime.flushPendingData(reason)
        }
        const resolvePersistentWorkingSet = () => bootstrapPersistentDatabase({
            store: runtime.store,
            onPhase: async (phase, locale) => {
                if (locale) changeLanguage(locale)
                await transition(
                    phase === 'storage' ? 'persistent-storage' : phase === 'compatibility'
                        ? 'plugin-compatibility-data' : 'persistent-database',
                    language.risuNest.startup[phase],
                )
            },
            prepareDatabase: prepareDatabaseForBootstrap,
            prepareRoot: preparePersistentRootForWorkingSet,
            projectScalableWorkingSet: (input) => projectCatalogWorkingSet(
                input.root,
                input.characters,
                createCatalogPresetWorkingSet(input.presetCatalog, input.activePreset),
            ),
        })
        stage = 'persistent-database'
        const local = await resolvePersistentWorkingSet()
        await transition('asset-repository', language.risuNest.startup.data)
        const assetRepositoryRevision = await activateNativeAssetRepository()
        if (assetRepositoryRevision !== null) local.revision = assetRepositoryRevision
        // The web build has no OS vault, so it keeps the account token in
        // localStorage. A native install holds it in the vault alone.
        const nativeCredentialVault = isTauri ? createNativeAccountCredentialVault() : null
        const nativeDeviceSettings = isTauri ? createNativeDeviceSettings() : null
        if (nativeCredentialVault) {
            localStorage.removeItem('fallbackRisuToken')
            localStorage.removeItem('risuauth')
        }
        const nativeCredential = nativeCredentialVault
            ? normalizeNativeOfficialAccountCredential(await nativeCredentialVault.read())
            : null
        if (isTauri) local.database.account = nativeCredential ?? undefined
        const installPersistentWorkingSet = (database: Database) => {
            workingSetResidency.clear()
            for (const character of database.characters) {
                if (isWorkingSetCharacterStub(character)) {
                    workingSetResidency.markCharacterReleased(character.chaId)
                }
            }
            setDatabase(database)
        }
        configurePersistentDataRuntime({
            projectWorkingSet(
                database,
                selectedCharacterId,
                selectedConversationId,
                activeCharacterIds,
                forceScalableProjection,
            ) {
                if (forceScalableProjection === false) return database
                const projected = isCatalogPresetWorkingSet(database.botPresets)
                    ? database
                    : projectCompleteScalableWorkingSet(
                        database,
                        selectedCharacterId,
                        runtime.revision,
                        activeCharacterIds,
                        selectedConversationId,
                    )
                for (const character of projected.characters) {
                    if (isWorkingSetCharacterStub(character)) {
                        workingSetResidency.markCharacterReleased(character.chaId)
                    } else {
                        workingSetResidency.reconcileConversationResidency(character)
                    }
                }
                return projected
            },
        })
        installPersistentWorkingSet(local.database)
        performance.mark('boot:local-data-ready')
        const uncachedNativeAccountStorage: AccountStorageCache = {
            getItem: async () => null,
            setItem: async () => undefined,
        }
        let nativeOfficialFlow: NativeOfficialAccountFlow | null = null
        let snapshotRequestReauthentication:
            NativeOfficialAccountFlowService['snapshotRequestReauthentication'] | null = null
        const nativeAccountStorageOptions = {
            databaseCache: uncachedNativeAccountStorage,
            assetCache: uncachedNativeAccountStorage,
        }
        const accountStorage = new AccountStorage(isTauri ? {
            ...nativeAccountStorageOptions,
            credentialRouting: {
                getToken: () => nativeOfficialFlow?.getToken(),
                reauthenticate: async (loginResult) => {
                    if (!snapshotRequestReauthentication) {
                        throw new Error('Native snapshot reauthentication is not configured')
                    }
                    await snapshotRequestReauthentication.reauthenticate(loginResult)
                },
            },
        } : {})
        const liveAccountStorage = isTauri
            ? new AccountStorage({
                ...nativeAccountStorageOptions,
                credentialRouting: {
                    getToken: () => nativeOfficialFlow?.getToken(),
                    reauthenticate: async (loginResult) => {
                        if (!nativeOfficialFlow) {
                            throw new Error('Native official account flow is not configured')
                        }
                        await nativeOfficialFlow.reauthenticate(loginResult)
                    },
                },
            })
            : accountStorage
        const nativeAssociation = nativeDeviceSettings
            ? await createNativeDeviceSettingsBag(
                nativeDeviceSettings,
                nativeOfficialAccountKeys.association,
            )
            : null
        const nativeAssetLedger = nativeDeviceSettings
            ? await createNativeDeviceSettingsBag(
                nativeDeviceSettings,
                nativeOfficialAccountKeys.assetLedger,
            )
            : null
        const associationStorage = nativeAssociation?.storage ?? localStorage
        const ledgerStorage = nativeAssetLedger?.storage ?? localStorage
        const officialAssetLedger = createAccountScopedOfficialAssetLedger(
            ledgerStorage,
            () => getDatabase().account?.id,
        )
        const flushNativeOfficialMetadata = async () => {
            await nativeAssociation?.flush()
            await nativeAssetLedger?.flush()
        }
        const clearNativeOfficialMetadata = async () => {
            await nativeAssociation?.clear()
            await nativeAssetLedger?.clear()
            officialAssetLedger.reset()
        }
        let nativePublicationRecovery: NativeOfficialPublicationRecovery | null = null
        const nativeDatabasePublisher = isTauri
            ? createNativeOfficialPublicationJobPublisher({
                account: accountStorage,
                baseUrl: hubURL,
                reconcilePendingPublications: async ({ accountId, revision }) => {
                    if (!nativePublicationRecovery) {
                        throw new Error('Native official publication recovery is not configured')
                    }
                    await nativePublicationRecovery.reconcile()
                    return nativePublicationRecovery.takeRecoveredPublication(
                        accountId,
                        revision,
                    )
                },
            })
            : undefined
        const officialAdapter = new OfficialAccountSnapshotAdapter({
            store: runtime.store,
            resolveBlobs: resolveBlobStore,
            account: accountStorage,
            cold: { readRemote: getAccountColdStorageItem },
            prepareCandidate: prepareDatabaseForPersistence,
            markPublished: () => undefined,
            ledger: officialAssetLedger,
            association: createOfficialAssociationMarkers(associationStorage),
            nativeDatabasePublisher,
            flushPublicationMetadata: nativeDatabasePublisher
                ? flushNativeOfficialMetadata
                : undefined,
            conflict: {
                resolve: async ({ remote, syncedAt }) => {
                    const lease = await runtime.store.acquireRevision(runtime.revision)
                    const summary = await withPersistentRevisionLease(
                        lease,
                        (reader) => summarizePinnedSyncConflict(reader, remote),
                    )
                    const details = [language.syncConflictDetected]
                    if (syncedAt !== null) {
                        details.push(language.syncConflictLastSynced
                            .replace('{date}', new Date(syncedAt).toLocaleString()))
                    }
                    if (summary.localOnlyNames.length > 0) {
                        details.push(language.syncConflictLocalOnly
                            .replace('{names}', formatNameList(summary.localOnlyNames)))
                    }
                    if (summary.remoteOnlyNames.length > 0) {
                        details.push(language.syncConflictRemoteOnly
                            .replace('{names}', formatNameList(summary.remoteOnlyNames)))
                    }
                    if (summary.changedNames.length > 0) {
                        details.push(language.syncConflictChanged
                            .replace('{names}', formatNameList(summary.changedNames)))
                    }
                    details.push(language.syncConflictBackupNotice)
                    const choice = await alertSelect([
                        language.syncConflictKeepLocal,
                        language.syncConflictLoadRemote,
                    ], details.join(' '))
                    return choice === '1' ? 'load-remote' : 'keep-local'
                },
                backup: async ({ side, bytes, characterCount }) => {
                    await getSyncConflictBackupStore().save({ side, bytes, characterCount })
                },
            },
        })
        let officialReconcilePublish = false
        await transition('account-bootstrap', language.risuNest.startup.account)
        const accountBootstrap = await initializeOfficialAccountBootstrap({
            isTauri,
            local,
            resolveWorkingSet: async () => resolvePersistentWorkingSet(),
            adapter: officialAdapter,
            readRemoteDatabase: () => accountStorage.readItem('database/database.bin', {
                progress: (value) => {
                    LoadingStatusState.text =
                        `Loading Remote Save File ${(value * 100).toFixed(2)}%`
                },
            }),
            markers: localStorage,
            accountMode: {
                get isAccount() {
                    return forageStorage.isAccount
                },
                set isAccount(enabled) {
                    forageStorage.setAccountModeForSession(enabled)
                },
            },
            configurePublisher: (officialPublisher) => {
                configurePersistentDataRuntime({ officialPublisher })
            },
            assetReader: createStructuredAccountAssetReader(accountStorage),
            configureAssetReader: configureOfficialAccountAssetReader,
            chooseExistingRemote: async () => await alertSelect([
                language.loadDataFromAccount,
                language.saveCurrentDataToAccount,
            ]) === '0' ? 'pull' : 'push',
            confirmInitialPush: async () =>
                await alertInput('to overwrite your data, type "RISUNEST"') === 'RISUNEST',
            installDatabase: installPersistentWorkingSet,
            initializeWorkingSet: (database) => initializeActiveWorkingSet(database),
            onRemoteError: (error) => {
                console.error(error)
                alertError(error instanceof Error ? error : String(error))
            },
            onPullSkipped: ({ conflict }) => {
                officialReconcilePublish = true
                console.warn(conflict
                    ? 'Official account sync conflict resolved by keeping local data; republishing over the remote save.'
                    : 'Official account pull skipped: local revisions were never published. Republishing local data.')
            },
        })
        if (nativeCredentialVault) {
            const assetReader = createStructuredAccountAssetReader(liveAccountStorage)
            configureOfficialAccountAssetReader(nativeCredential ? assetReader : null)
            const service = createNativeOfficialAccountFlowService({
                credentialVault: nativeCredentialVault,
                adapter: officialAdapter,
                initialCredential: nativeCredential,
                flushPendingData: (reason) => runtime.flushPendingData(reason),
                getRevision: () => runtime.revision,
                restart: restartNativeApp,
                clearLegacyFallback: () => localStorage.removeItem('fallbackRisuToken'),
                setRouting: (credential) => {
                    getDatabase().account = credential ?? undefined
                    forageStorage.setAccountModeForSession(false)
                    configurePersistentDataRuntime({ officialPublisher: null })
                    configureOfficialAccountAssetReader(credential ? assetReader : null)
                },
                flushMetadata: flushNativeOfficialMetadata,
                clearMetadata: clearNativeOfficialMetadata,
                resetAccountSession: resetAccountStorageSession,
                nativeRestore: async (initialCredential) => {
                    let credential = initialCredential
                    for (let attempt = 0; attempt < 3; attempt += 1) {
                        try {
                            const result = await runNativeOfficialAccountSnapshotRestore(
                                runtime,
                                {
                                    baseUrl: hubURL,
                                    credential: {
                                        kind: 'risu-auth',
                                        token: credential.token,
                                    },
                                },
                                { afterRefresh: loadPluginsAfterAuthoritativeRestore },
                            )
                            if (result.kind !== 'activated') return result
                            officialAdapter.rememberNativeActivation(
                                credential.id,
                                result.revision,
                                result.sourceSha256,
                            )
                            return { kind: 'activated', revision: result.revision }
                        } catch (error) {
                            if (
                                !(error instanceof NativeFileJobError)
                                || error.code !== 'reauthentication-needed'
                                || attempt >= 2
                            ) throw error
                            if (!snapshotRequestReauthentication) {
                                throw new Error('Native snapshot reauthentication is not configured')
                            }
                            credential = await snapshotRequestReauthentication.reauthenticate(
                                await alertLogin(),
                            )
                        }
                    }
                    throw new Error('Native official account restore did not complete')
                },
            })
            nativeOfficialFlow = service.flow
            snapshotRequestReauthentication = service.snapshotRequestReauthentication
            configureNativeOfficialAccountFlow(service.flow)
            nativePublicationRecovery = createNativeOfficialPublicationRecovery(
                recoveredNativeFileJobs.pendingOfficialPublications,
                {
                    activeAccountId: () => getDatabase().account?.id ?? null,
                    account: accountStorage,
                    adapter: officialAdapter,
                    flushMetadata: flushNativeOfficialMetadata,
                },
            )
            await nativePublicationRecovery.reconcile()
        } else {
            configureNativeOfficialAccountFlow(null)
        }
        performance.mark('boot:account-ready')
        if (officialReconcilePublish && accountBootstrap.officialEnabled) {
            publishCurrentOfficialRevision().catch((error) => {
                console.error('Official reconcile publish failed', error)
            })
        }
        if (isTauri) {
            try {
                const { installExternalStorageProduction } = await import(
                    './storage/sync/external/production'
                )
                await installExternalStorageProduction()
            } catch (error) {
                console.error('External storage scheduler failed to start', error)
            }
        }
        let heldExitRevision: number | undefined
        let selectedExitDrain: SyncExitDrainAdapter | null = null
        const syncExitCoordinator = createSyncExitCoordinator({
            async acquireEditFence() {
                const token = await runtime.capturePersistentMutationToken(
                    'normal-exit-fence',
                    { publishOfficial: false },
                )
                const fence = await runtime.acquireDestructiveReplacementFence(token)
                heldExitRevision = fence.revision
                return fence
            },
            flushLocal: () => runtime.flushPendingDataLocally('normal-exit'),
            checkpointLocal: async () => {
                if (!isTauri) return
                const { checkpointNativePersistentStore } = await import(
                    './storage/nativePersistentMaintenance'
                )
                await checkpointNativePersistentStore('truncate')
            },
            async captureTarget() {
                if (heldExitRevision === undefined) {
                    throw new Error('Normal exit revision was not fenced')
                }
                const capture = await getExternalStorageBridge().captureExitTarget()
                const revision = Number(capture.revision)
                if (!Number.isSafeInteger(revision) || revision !== heldExitRevision) {
                    throw new Error('Normal exit revision changed after the edit fence')
                }
                const selection = capture.selection
                if (selection.kind !== 'none' && selection.decisionRequired) {
                    throw { code: 'sync-selection-decision-required' }
                }
                if (selection.kind !== 'none' && !selection.connectionId) {
                    throw { code: 'sync-selection-invalid' }
                }
                selectedExitDrain = null
                let selectionId = `none:${selection.selectionEpoch}`
                if (
                    !selection.decisionRequired
                    && selection.kind === 'server'
                    && selection.connectionId
                ) {
                    selectionId = `server:${selection.connectionId}:${selection.selectionEpoch}`
                    selectedExitDrain = createServerSyncExitDrainAdapter(selectionId)
                } else if (
                    !selection.decisionRequired
                    && selection.kind === 'external'
                    && selection.connectionId
                ) {
                    selectionId = `external:${selection.connectionId}:${selection.selectionEpoch}`
                    const {
                        getExternalStorageSyncExitDrainAdapter,
                        installExternalStorageProduction,
                    } = await import(
                        './storage/sync/external/production'
                    )
                    await installExternalStorageProduction()
                    selectedExitDrain = getExternalStorageSyncExitDrainAdapter(capture)
                    if (!selectedExitDrain || selectedExitDrain.id !== selectionId) {
                        throw new Error('Selected external synchronization target is unavailable')
                    }
                } else if (
                    selection.kind === 'none'
                    && forageStorage.isAccount
                    && hasPendingOfficialPublication()
                ) {
                    selectionId = 'official-account'
                    selectedExitDrain = {
                        id: selectionId,
                        async drain() {
                            await runtime.publishCurrentOfficialRevision()
                            return hasPendingOfficialPublication()
                                ? {
                                    kind: 'blocked',
                                    reason: 'official-account-sync-pending',
                                }
                                : { kind: 'complete' }
                        },
                        cancel: async () => {},
                    }
                }
                return {
                    revision,
                    libraryEpoch: capture.libraryEpoch,
                    selectionEpoch: selection.selectionEpoch,
                    selectionId,
                }
            },
            selectedDrain: () => selectedExitDrain,
            reportError: (error) =>
                console.error('Normal exit drain failed', error),
        })
        configureSyncExitCoordinator(syncExitCoordinator)
        disposeLifecycleCommitListeners ??= registerLifecycleCommitListeners(
            flushLifecycle,
            syncExitCoordinator,
        )

        if (
            isTauriDesktop &&
            nativePlatform() === 'macos' &&
            !disposeMacosLifecycle
        ) {
            const { registerMacosLifecycle } = await import(
                './storage/macosLifecycle'
            )
            disposeMacosLifecycle = await registerMacosLifecycle({
                coordinator: syncExitCoordinator,
            })
        }

        if (
            isTauriDesktop
            && nativePlatform() !== 'macos'
            && appWindow
            && !disposeWindowCloseDrain
        ) {
            disposeWindowCloseDrain = await registerWindowCloseDrain(
                appWindow,
                syncExitCoordinator,
            )
        }

        if (isTauriDesktop) await changeFullscreen()

        if (!isTauri && !excluded('sync')) {
            await transition('service-worker', language.risuNest.startup.serviceWorker)
            if (navigator.serviceWorker) {
                setUsingSw(true)
                await registerSw()
            } else {
                setUsingSw(false)
            }
        }
        if (isTauri)
            void initializeNativeLocalUrls((id) => {
                if (getDatabase().didFirstSetup) void downloadRisuHub(id)
            })
        if (getDatabase().didFirstSetup) void characterURLImport()

        await transition('format-update', language.risuNest.startup.data)
        performance.mark('boot:cold-storage-ready')
        await transition('plugins', language.risuNest.startup.plugins)
        let pluginsLoaded = excluded('plugins')
        try {
            if (!pluginsLoaded) await loadPlugins()
            pluginsLoaded = true
        } catch (error) {
            console.error(error)
        }
        if (pluginsLoaded && recoveredNativeRestoreJobs.length > 0) {
            try {
                await acknowledgeRecoveredNativeRestores(recoveredNativeRestoreJobs)
            }
            catch (error) {
                console.error('Native restore acknowledgement failed', error)
            }
        }
        performance.mark('boot:plugins-ready')
        if (!isTauri && getDatabase().account) {
            await transition('account-data', language.risuNest.startup.account)
            try {
                await loadRisuAccountData()
            } catch (error) {
                console.error(error)
            }
        }
        try {
            const isInStandaloneMode = window.matchMedia('(display-mode: standalone)').matches ||
                (window.navigator as Navigator & { standalone?: boolean }).standalone ||
                document.referrer.includes('android-app://')
            if (isInStandaloneMode) await navigator.storage.persist()
        } catch {}

        const database = getDatabase()
        await transition('ui-state', language.risuNest.startup.ui)
        updateColorScheme()
        if (!excluded('theme')) updateTextThemeAndCSS()
        updateAnimationSpeed()
        updateHeightMode()
        updateErrorHandling()
        updateGuisize()
        if (!markers.getItem('nightlyWarned') && import.meta.env.VITE_RISU_NIGHTLY_BUILD === 'TRUE') {
            alertMd(language.nightlyWarning)
            await waitAlert()
            markers.setItem('nightlyWarned', 'true')
            await markers.flush()
        }
        if (database.botSettingAtStart) botMakerMode.set(true)
        if (
            (database.betaMobileGUI && window.innerWidth <= 800) ||
            import.meta.env.VITE_RISU_LITE === 'TRUE'
        ) {
            initMobileGesture()
            MobileGUI.set(true)
        }
        registerAndroidScreenshotPublicationRecovery()
        await initializeIOSNative()
        LoadingStatusState.startedAt = null
        loadedStore.set(true)
        void finishBoot()
        performance.mark('boot:interactive')
        selectedCharID.set(-1)
        await yieldToUi()
        startObserveDom()
        registerModelDynamic()
        await saveDb()
        registerAndroidRisuSaveRoute()
        if (isTauri) {
          schedulePeriodicNativeSnapshot();
          installIOSPersistenceLifecycle((reason) => flushLifecycle(reason, true));
        }
        if (!excluded('modules')) moduleUpdate()
        void alertTOS().then((accepted) => {
            if (accepted === false) location.reload()
        })
    } catch (error) {
        console.error('RisuNest startup failed', error)
        const failure = classifyBootFailure(error, stage)
        bootFailure.set(failure)
        // The classified store failures are explained by the startup panel, so
        // an extra error modal on top of it would only get in the way. Anything
        // else can still fail after the app turned interactive, where no panel
        // is shown, so those keep the modal.
        if (failure.kind === 'unknown' && failure.stage !== 'native-setup') alertError(error)
    } finally {
        LoadingStatusState.startedAt = null
    }
}


/**
 * Registers the service worker and initializes it.
 */
async function registerSw() {
    await navigator.serviceWorker.register("/sw.js", {
        scope: "/"
    });
    await sleep(100);
    const da = await fetch('/sw/init');
    if (!(da.status >= 200 && da.status < 300)) {
        location.reload();
    }
}

/**
 * Updates the error handling by adding custom handlers for errors and unhandled promise rejections.
 */
function updateErrorHandling() {
    const errorHandler = (event: ErrorEvent) => {
        console.error(event.error);
        if(!(event.error.target instanceof Worker)){
            alertError(event.error);            
        }
    };
    const rejectHandler = (event: PromiseRejectionEvent) => {
        console.error(event.reason);
        alertError(event.reason);
    };
    window.addEventListener('error', errorHandler);
    window.addEventListener('unhandledrejection', rejectHandler);
}

/**
 * Updates the height mode of the document based on the value stored in the database.
 */
function updateHeightMode() {
    const db = getDatabase()
    const root = document.querySelector(':root') as HTMLElement;
    switch (db.heightMode) {
        case 'auto':
            root.style.setProperty('--risu-height-size', '100%');
            break
        case 'vh':
            root.style.setProperty('--risu-height-size', '100vh');
            break
        case 'dvh':
            root.style.setProperty('--risu-height-size', '100dvh');
            break
        case 'lvh':
            root.style.setProperty('--risu-height-size', '100lvh');
            break
        case 'svh':
            root.style.setProperty('--risu-height-size', '100svh');
            break
        case 'percent':
            root.style.setProperty('--risu-height-size', '100%');
            break
    }
}

/**
 * Purges chunks of data that are not needed.
 */
async function cleanChunks() {
    const db = getDatabase()
    if (hasIncompletePersistentWorkingSet(db, workingSetResidency)) return
    if (db.account?.useSync) {
        return
    }

    const uncleanable = new Set(await getUncleanables(db))
    const blobStore = await resolveBlobStore()
    if (isTauri) {
        const assets = await readDir('assets', { baseDir: BaseDirectory.AppData })
        console.log(assets)
        for (const asset of assets) {
            try {
                const n = getBasename(asset.name)
                if (!uncleanable.has(n)) {
                    await blobStore.remove('assets/' + asset.name)
                    invalidateAssetSourceCache('assets/' + asset.name)
                }
            } catch (error) {
                console.log('error', asset.name)
            }
        }

        
        if(!await exists('remotes', { baseDir: BaseDirectory.AppData })) {
            await mkdir('remotes', { baseDir: BaseDirectory.AppData })
        }

        const remotes = await readDir('remotes', { baseDir: BaseDirectory.AppData })

        const remoteUncleanables = new Set<string>(
            db.characters.map((v) => v.chaId)
        )
        for (const remote of remotes) {
            try {
                const remoteFileName = getBasename(remote.name)
                const remotePayloadName = getRemoteSavePayloadName(remoteFileName)
                if(!remotePayloadName){
                    continue
                }
                const fexists = remoteUncleanables.has(remotePayloadName)
                if(!fexists){

                    const metaPath = 'remotes/' + remote.name + '.meta'
                    let metaExists = false
                    let metaLastUsed:unknown
                    try {
                        metaExists = await exists(metaPath, { baseDir: BaseDirectory.AppData })
                        if (metaExists) {
                            const meta = await readFile(metaPath, { baseDir: BaseDirectory.AppData })
                            const metaJson = JSON.parse(new TextDecoder().decode(meta))
                            metaLastUsed = metaJson.lastUsed
                        }
                    } catch (error) {}

                    const cleanupAction = getRemoteSaveCleanupAction({
                        fileName: remoteFileName,
                        activeCharacterIds: remoteUncleanables,
                        hasMeta: metaExists,
                        metaLastUsed
                    })
                    if(cleanupAction === 'create-meta'){
                        const metaJson = {
                            lastUsed: Date.now()
                        }
                        await writeFile(metaPath, new TextEncoder().encode(JSON.stringify(metaJson)), { baseDir: BaseDirectory.AppData })
                    }
                    else if(cleanupAction === 'delete'){
                        await remove('remotes/' + remote.name, { baseDir: BaseDirectory.AppData })
                        await remove(metaPath, { baseDir: BaseDirectory.AppData })
                    }
                }
            } catch (error) {
                console.log('error', remote.name)
            }
        }
    }
    else {
        const indexes = await forageStorage.keys()
        const characterIds = new Set<string>(
            db.characters.map((v) => v.chaId)
        )
        for (const asset of indexes) {
            if (asset.startsWith('assets/')) {
                const n = getBasename(asset)
                if(!uncleanable.has(n)) {
                    await blobStore.remove(asset)
                    invalidateAssetSourceCache(asset)
                }
            }
            else if (asset.endsWith('.meta')){
                continue
            }
            else if (asset.startsWith('remotes/')) {
                const name = getBasename(asset).slice(0, -10) //remove .local.bin
                const exists = characterIds.has(name)
                if(!exists){
                    let okayToDelete = false
                    try {
                        const metaPath = asset + '.meta'
                        const metaExists = (await forageStorage.keys()).includes(metaPath)
                        if (metaExists) {
                            const metaData: Uint8Array = await forageStorage.getItem(metaPath) as unknown as Uint8Array
                            const metaJson = JSON.parse(new TextDecoder().decode(metaData))
                            const lastUsed = metaJson.lastUsed as number
                            if(Date.now() - lastUsed > 1000 * 60 * 60 * 24 * 7) { //not used for 7 days
                                okayToDelete = true
                            }
                        }
                        else{
                            //write meta for next time
                            const metaJson = {
                                lastUsed: Date.now()
                            }
                            await forageStorage.setItem(metaPath, new TextEncoder().encode(JSON.stringify(metaJson)))
                        }
                    } catch (error) {}
                    if (okayToDelete) {
                        await forageStorage.removeItem(asset)
                    }
                }
            }
        }
    }
}


/**
 * Assigns unique IDs to characters and chats.
 */
