import { initializeAppLock } from './appLock/appLock.svelte'
import { classifyBootFailure, type BootStage } from './bootFailureClassification'
import {
    writeFile,
    readFile,
    exists,
    mkdir,
    readDir,
    remove
} from "@tauri-apps/plugin-fs"
import { join } from "@tauri-apps/api/path"
import { nativeDataPath } from "./storage/nativePaths"
import { changeFullscreen, sleep } from "./util"
import { get } from "svelte/store";
import { setDatabase, getDatabase, type Database } from "./storage/database.svelte";
import { getStartupExclusions, loadDeviceSettings } from "./storage/deviceSettings";
import { recordNativeLogError, setNativeLogFileEnabled } from "./nativeLog";
import { generationDiagnostics } from './generationDiagnostics'
import { registerRuntimeErrorHandlers } from "./runtimeErrors";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { MobileGUI, botMakerMode, selectedCharID, loadedStore, LoadingStatusState, bootFailure } from "./stores.svelte";
import { afterPluginsStarted, keepPluginsOffForThisStart, loadPlugins, loadPluginsAfterAuthoritativeRestore } from "./plugins/plugins.svelte";
import { setPluginStartupSyncExcluded } from './plugins/pluginStartupGate';
import { alertActionConfirm, alertConfirm, alertError, alertInput, alertLogin, alertMd, alertNormal, alertSelect, alertToast, alertTOS, waitAlert } from "./alert";
import { backgroundErrorMessage } from "./storage/backgroundErrorMessage";
import { applyHubSelection, characterURLImport, downloadRisuHub, hubURL } from "./characterCards";
import { initializeNativeLocalUrls } from "./nativeLocalUrls";
import { loadRisuAccountData } from "./drive/accounter";
import { updateAnimationSpeed } from "./gui/animation";
import { updateColorScheme, updateTextThemeAndCSS } from "./gui/colorscheme";
import { changeLanguage, language } from "src/lang";
import { startObserveDom } from "./observer.svelte";
import { updateHeightMode } from "./gui/heightMode";
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
import { isTauri, isTauriAndroid, isTauriDesktop, isTauriIOS } from "./platform";
import { expireMobileDeferredHolds, mobileDeferredHoldsSettled } from "./mobileBackgroundTask";
import { setGenerationBackground } from "./process/generationState";
import { beginGenerationBackground } from "./process/generationBackground";
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
import { createLifecycleFlush } from "./storage/lifecycleFlush";
import { forgetDisplayRecovery, installDisplayRecovery, listenForDisplayWorkResume, suspendDisplayWork } from "./displayRecovery.svelte";
import { releaseIdleTransformerModels } from "./process/transformers";
import { forgetInlayProviderImages } from "./process/files/inlayProviderImage";
import { platform as nativePlatform } from '@tauri-apps/plugin-os'
import { resolveBlobStore } from "./storage/platformBlobStore";
import {
    OfficialAccountSnapshotAdapter,
    createOfficialAssociationMarkers,
    type OfficialRestoreOmissions,
} from "./storage/sync/officialAccountSnapshot";
import { createNativeOfficialAccountAssets } from "./storage/sync/nativeOfficialAccountAssets";
import { getSyncConflictBackupStore } from "./storage/sync/syncConflictBackup";
import { formatNameList, summarizePinnedSyncConflict } from "./storage/sync/syncConflictSummary";
import { withPersistentRevisionLease } from "./storage/persistentRecordIterator";
import {
    initializePersistentStorage,
} from "./storage/persistentStorageRuntime";
import { compactRequestedStoreAtStart, showStoreCompactionNotice } from "./storage/storeCompaction";
import {
    acknowledgeRecoveredNativeRestores,
    reconcileNativeFileJobsBeforeBootstrap,
    shouldReconcileNativeFileJobs,
} from "./storage/nativeFileJobRecovery";
import { refreshDeviceStateAfterRestore } from "./storage/deviceStateRestore";
import { registerAndroidRisuSaveRoute } from "./storage/androidRisuSaveRouteProduction.svelte";
import { reportInterruptedIOSBackupSourcesAtStart } from "./storage/iosFiles";
import { isAndroidSafFileJobsEnabled } from "./storage/androidSafBridge";
import {
    describeScreenshotPublicationError,
    listenRecoveredAndroidScreenshotPublications,
} from "./nativeScreenshotArchiveWriter";
import { initializeIOSNative, installIOSPersistenceLifecycle } from "./iosNative";
import { checkpointNativePersistentStore, restartNativeApp, schedulePeriodicNativeSnapshot } from "./storage/nativePersistentMaintenance";
import { yieldToUi } from './ui/yieldToUi'
import { markBootStage, markBootSuspect } from './storage/bootAttempt'
import {
    finishBoot,
    isStartupExcluded,
    takeRendererRecovery,
    type RecoveryExclusion,
} from './storage/recoveryMode.svelte'
import {
    initializeOfficialAccountBootstrap,
} from "./storage/sync/officialAccountBootstrap";
import { createAccountScopedOfficialAssetLedger, createNativeOfficialAssetLedger } from "./storage/sync/officialAssetLedger";
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
    restoreNativeOfficialAccountCandidate,
    type NativeOfficialAccountFlow,
    type NativeOfficialAccountFlowService,
    nativeOfficialAccountKeys,
    normalizeNativeOfficialAccountCredential,
} from "./storage/sync/nativeOfficialAccountFlow";
import {
    NativeFileJobError,
    runNativeOfficialAccountSnapshotRestore,
} from "./storage/nativeFileJobs";
import { createNativeOfficialAssetPublisher, createNativeOfficialPublicationJobPublisher } from "./storage/sync/nativeOfficialPublicationJob";
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
import { createServerSyncExitDrainAdapter, holdServerSyncAfterRestore, getServerSyncController, initializeNativeSyncBindings, installServerSyncProduction, disposeNativeSyncBindings } from './storage/sync/serverSyncProduction'
import {
    retainableReplacementFence,
    type RetainableReplacementFence,
} from './storage/retainableReplacementFence'

const appWindow = isTauri ? getCurrentWebviewWindow() : null
let disposeLifecycleCommitListeners: (() => void) | undefined
let disposeMacosLifecycle: (() => void) | undefined
let disposeDisplayWorkResume: (() => void) | undefined
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
 * Loads the application data.
 */
export async function loadData() {
    if (get(loadedStore)) return
    generationDiagnostics.startDocument()
    let stage: BootStage = 'startup'
    LoadingStatusState.startedAt = performance.now()
    bootFailure.set(null)
    const transition = async (nextStage: BootStage, text: string) => {
        stage = nextStage
        markBootStage(nextStage)
        LoadingStatusState.text = text
        await yieldToUi()
    }
    const excluded = (name: RecoveryExclusion): boolean =>
        isStartupExcluded(name, getStartupExclusions())
    LoadingStatusState.text = language.risuNest.startup.storage
    try {
        await initializeIOSNative()
        if (isTauri) {
            stage = 'native-setup'
            await checkNativeStartupStatus()
        }
        if (isTauri) {
            await transition('app-data-directories', language.risuNest.startup.storage)
        } else {
            stage = 'browser-storage'
            await forageStorage.Init()
        }

        await transition('persistent-storage', language.risuNest.startup.storage)
        const storeCompaction = isTauri
            ? await compactRequestedStoreAtStart((text) => { LoadingStatusState.text = text })
            : null
        LoadingStatusState.text = language.risuNest.startup.storage
        await initializePersistentStorage()
        if (isTauri) {
            stage = 'device-settings'
            await initializeDeviceMarkers()
            await initializeAppLock()
        }
        const markers = getDeviceMarkers()
        // Chats whose display work the last run left recorded open in recovery.
        await installDisplayRecovery(markers)
        disposeDisplayWorkResume ??= listenForDisplayWorkResume()
        applyHubSelection(markers)
        const deviceSettings = loadDeviceSettings(markers)
        const pluginsExcludedAtStart = excluded('plugins')
        setPluginStartupSyncExcluded(excluded('sync'))
        // A restore or a received change later in this run must not start the plugins either.
        if (pluginsExcludedAtStart) keepPluginsOffForThisStart()
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
                reconcileRestores: shouldReconcileNativeFileJobs(isTauri),
            })
            : {
                pendingRestoreAcknowledgements: [],
                pendingOfficialPublications: [],
                interruptedRestores: [],
                libraryRestoreCommitted: false,
                pendingSnapshotBodies: [],
            }
        if (recoveredNativeFileJobs.pendingRestoreAcknowledgements.length > 0) {
            await refreshDeviceStateAfterRestore()
        }
        if (recoveredNativeFileJobs.libraryRestoreCommitted) holdServerSyncAfterRestore()
        const recoveredNativeRestoreJobs =
            recoveredNativeFileJobs.pendingRestoreAcknowledgements
        const runtime = getPersistentDataRuntime()
        const { flush: flushLifecycle, flushForLifecycle } = createLifecycleFlush({
            runtime,
            isTauriAndroid,
            isTauriDesktop,
            nativePlatform,
            forgetInlayProviderImages,
            releaseIdleTransformerModels,
            suspendDisplayWork,
        })
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
        // The web build has no OS vault, so it keeps the account token in
        // localStorage. A native install holds it in the vault alone.
        const nativeCredentialVault = isTauri ? createNativeAccountCredentialVault() : null
        const nativeDeviceSettings = isTauri ? createNativeDeviceSettings() : null
        if (nativeCredentialVault) {
            localStorage.removeItem('fallbackRisuToken')
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
                getAccountId: () => nativeOfficialFlow?.getAccountId(),
                getToken: () => nativeOfficialFlow?.getToken(),
                reauthenticate: async (loginResult) => {
                    if (!snapshotRequestReauthentication) {
                        throw new Error('Native snapshot reauthentication is not configured')
                    }
                    await snapshotRequestReauthentication.reauthenticate(loginResult)
                },
            },
        } : {})
        const nativeAssociation = nativeDeviceSettings
            ? await createNativeDeviceSettingsBag(
                nativeDeviceSettings,
                nativeOfficialAccountKeys.association,
            )
            : null
        const associationStorage = nativeAssociation?.storage ?? localStorage
        const getNativeAccountId = () => nativeOfficialFlow
            ? nativeOfficialFlow.getAccountId()
            : nativeCredential?.id ?? null
        const nativeAssetLedger = nativeDeviceSettings
            ? await createNativeOfficialAssetLedger(
                nativeDeviceSettings,
                nativeOfficialAccountKeys.assetLedger,
                () => getNativeAccountId() ?? undefined,
            )
            : null
        const officialAssetLedger = nativeAssetLedger ?? createAccountScopedOfficialAssetLedger(
            localStorage,
            () => getDatabase().account?.id,
        )
        const flushNativeOfficialMetadata = async () => {
            const results = await Promise.allSettled([nativeAssociation?.flush(), nativeAssetLedger?.flush()])
            const failures = results.flatMap(result => result.status === 'rejected' ? [result.reason] : [])
            if (failures.length) throw new AggregateError(failures, 'Official account metadata could not be saved')
        }
        const clearNativeOfficialMetadata = async () => {
            const results = await Promise.allSettled([nativeAssociation?.clear(), nativeAssetLedger?.clearAll()])
            const failures = results.flatMap(result => result.status === 'rejected' ? [result.reason] : [])
            if (failures.length) throw new AggregateError(failures, 'Official account metadata could not be cleared')
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
        const pluginOmissionWarnings = (omissions?: OfficialRestoreOmissions) => [
            ...(omissions?.pluginStorage
                ? [language.portableBackup.omittedRestoreData.pluginStorage(omissions.pluginStorage)]
                : []),
            ...(omissions?.oversizedPluginStorage
                ? [language.portableBackup.omittedRestoreData.oversizedPluginStorage(omissions.oversizedPluginStorage)]
                : []),
        ]
        const officialAdapter = new OfficialAccountSnapshotAdapter({
            store: runtime.store,
            resolveBlobs: resolveBlobStore,
            account: accountStorage,
            cold: { readRemote: getAccountColdStorageItem },
            prepareCandidate: prepareDatabaseForPersistence,
            confirmOmissions: isTauri
                ? undefined
                : (omissions) => alertActionConfirm({
                    title: language.loadDataFromAccount,
                    description: pluginOmissionWarnings(omissions).join('\n\n'),
                    actionLabel: language.confirm,
                    cancelLabel: language.cancel,
                }),
            restoreCandidate: isTauri
                ? (candidate, expectedRevision, signal, omissions) => restoreNativeOfficialAccountCandidate(
                    runtime, candidate, expectedRevision, signal,
                    [language.risuNest.backup.officialRestoreInlayWarning, ...pluginOmissionWarnings(omissions)],
                )
                : undefined,
            markPublished: () => undefined,
            ledger: officialAssetLedger,
            association: createOfficialAssociationMarkers(associationStorage),
            nativeDatabasePublisher,
            nativeAssetPublisher: isTauri
                ? createNativeOfficialAssetPublisher({ account: accountStorage, baseUrl: hubURL })
                : undefined,
            getAccountId: isTauri ? getNativeAccountId : undefined,
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
        if (!excluded('account')) await transition('account-bootstrap', language.risuNest.startup.account)
        const accountBootstrap = excluded('account') ? { officialEnabled: false } : await initializeOfficialAccountBootstrap({
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
            chooseExistingRemote: async () => {
                const choice = await alertSelect([
                    language.loadDataFromAccount,
                    language.saveCurrentDataToAccount,
                    language.cancelAccountSync,
                ])
                return choice === '0' ? 'pull' : choice === '2' ? 'cancel' : 'push'
            },
            confirmInitialPush: async () =>
                await alertInput('to overwrite your data, type "RISUNEST"') === 'RISUNEST',
            installDatabase: installPersistentWorkingSet,
            initializeWorkingSet: (database) => initializeActiveWorkingSet(database),
            disableAccountSync: () => {
                const account = getDatabase().account
                if (account) account.useSync = false
            },
            onRemoteError: (error) => {
                console.error(error)
                alertError(backgroundErrorMessage(error))
            },
            onPullSkipped: ({ conflict }) => {
                officialReconcilePublish = true
                console.warn(conflict
                    ? 'Official account sync conflict resolved by keeping local data; republishing over the remote save.'
                    : 'Official account pull skipped: local revisions were never published. Republishing local data.')
            },
        })
        if (nativeCredentialVault) {
            configureOfficialAccountAssetReader(null)
            const restoredAssets = createNativeOfficialAccountAssets({
                settings: nativeDeviceSettings!, store: runtime.store, resolveBlobs: resolveBlobStore,
                account: accountStorage, ledger: officialAssetLedger, flushMetadata: flushNativeOfficialMetadata, baseUrl: hubURL,
                getAccountId: getNativeAccountId,
            })
            const completeRestoreAssets = restoredAssets.complete
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
                    configureOfficialAccountAssetReader(null)
                },
                flushMetadata: flushNativeOfficialMetadata,
                clearMetadata: clearNativeOfficialMetadata,
                resetAccountSession: resetAccountStorageSession,
                prepareRestoreAssets: restoredAssets.prepare,
                bindRestoreAssets: restoredAssets.bind,
                inspectRestoreAssets: restoredAssets.inspect,
                stopRestoreAssets: restoredAssets.stop,
                completeRestoreAssets,
                clearRestoreAssets: restoredAssets.cancelPreparation,
                nativeRestore: async (initialCredential, options) => {
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
                                { ...options, afterRefresh: loadPluginsAfterAuthoritativeRestore },
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
                    activeAccountId: getNativeAccountId,
                    account: accountStorage,
                    adapter: officialAdapter,
                    flushMetadata: flushNativeOfficialMetadata,
                },
            )
            if (!excluded('account')) await nativePublicationRecovery.reconcileSettled()
        } else {
            configureNativeOfficialAccountFlow(null)
        }
        performance.mark('boot:account-ready')
        if (isTauri) {
            initializeNativeSyncBindings()
            window.addEventListener('pagehide', disposeNativeSyncBindings, { once: true })
        }
        // The exit drain follows what this start did, not an exclusion cleared later in the session.
        const syncExcludedAtStart = excluded('sync')
        if (isTauri && !syncExcludedAtStart) {
            await transition('drive-sync', language.risuNest.startup.account)
            await installServerSyncProduction()
            try {
                const { installExternalStorageProduction } = await import(
                    './storage/sync/external/production'
                )
                await installExternalStorageProduction()
            } catch (error) {
                console.error('External storage scheduler failed to start', error)
            }
        } else if (isTauri) {
            // Sync stays stopped, but its settings can still turn a sync target on or off.
            await transition('drive-sync', language.risuNest.startup.account)
            try {
                await installServerSyncProduction({ resumeBound: false })
            } catch (error) {
                console.error('Server sync transport failed to register', error)
            }
            try {
                const { installExternalSyncTransports } = await import(
                    './storage/sync/external/production'
                )
                await installExternalSyncTransports()
            } catch (error) {
                console.error('External sync transports failed to register', error)
            }
        }
        if (isTauri) {
            try {
                const { installProductionSyncStatus } = await import('./storage/sync/syncStatusProduction')
                installProductionSyncStatus()
            } catch (error) {
                console.error('Sync status failed to start', error)
            }
        }
        let heldExitRevision: number | undefined
        let heldExitFence: RetainableReplacementFence | undefined
        let selectedExitDrain: SyncExitDrainAdapter | null = null
        const syncExitCoordinator = createSyncExitCoordinator({
            async acquireEditFence() {
                const token = await runtime.capturePersistentMutationToken(
                    'normal-exit-fence',
                    { publishOfficial: false },
                )
                const fence = retainableReplacementFence(
                    await runtime.acquireDestructiveReplacementFence(token),
                )
                heldExitRevision = fence.revision
                heldExitFence = fence
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
                // A start that left sync off drains only a target the user started during the session.
                const syncStopped = syncExcludedAtStart && !(
                    selection.kind === 'server'
                        ? !getServerSyncController().snapshot().paused
                        : selection.kind === 'external' && !!selection.connectionId
                            && (await import('./storage/sync/external/lwwProduction')).isExternalLwwRunning(selection.connectionId)
                )
                const syncDisabled = syncStopped || (
                    selection.kind !== 'none' && (selection.paused || (
                        selection.kind === 'server' && getServerSyncController().snapshot().paused
                    ))
                )
                if (!syncDisabled && selection.kind !== 'none' && !selection.connectionId) {
                    throw { code: 'sync-selection-invalid' }
                }
                selectedExitDrain = null
                let selectionId = `none:${selection.selectionEpoch}`
                if (
                    !syncDisabled
                    && selection.kind === 'server'
                    && selection.connectionId
                ) {
                    selectionId = `server:${selection.connectionId}:${selection.selectionEpoch}`
                    selectedExitDrain = createServerSyncExitDrainAdapter(
                        selectionId,
                        heldExitFence,
                    )
                } else if (
                    !syncDisabled
                    && selection.kind === 'external'
                    && selection.connectionId
                ) {
                    selectionId = `external:${selection.connectionId}:${selection.selectionEpoch}`
                    const {
                        getExternalStorageSyncExitDrainAdapter,
                        installExternalStorageProduction,
                        installExternalSyncTransports,
                    } = await import(
                        './storage/sync/external/production'
                    )
                    // A start that left sync off also left automatic backups off, so exit needs only the transports.
                    await (syncExcludedAtStart ? installExternalSyncTransports() : installExternalStorageProduction())
                    selectedExitDrain = getExternalStorageSyncExitDrainAdapter(
                        capture,
                        heldExitFence,
                    )
                    if (!selectedExitDrain || selectedExitDrain.id !== selectionId) {
                        throw new Error('Selected external synchronization target is unavailable')
                    }
                } else if (
                    selection.kind === 'none'
                    && !excluded('account')
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
            // The app handled the exit, so its display work is not frozen and must not be marked at the next launch.
            prepareExit: () => suspendDisplayWork('normal-exit'),
            reportError: (error) =>
                console.error('Normal exit drain failed', error),
        })
        configureSyncExitCoordinator(syncExitCoordinator)
        disposeLifecycleCommitListeners ??= registerLifecycleCommitListeners(
            flushForLifecycle,
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
                saveLocally: async () => {
                    await flushLifecycle('stop', true)
                    await checkpointNativePersistentStore('truncate')
                },
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
        if (recoveredNativeRestoreJobs.length > 0) {
            if (excluded('plugins')) {
                try { await acknowledgeRecoveredNativeRestores(recoveredNativeRestoreJobs) }
                catch (error) { console.error('Native restore acknowledgement failed', error) }
            } else {
                const stop = afterPluginsStarted(async () => {
                    try { await acknowledgeRecoveredNativeRestores(recoveredNativeRestoreJobs); stop() }
                    catch (error) { console.error('Native restore acknowledgement failed', error) }
                })
            }
        }
        try { if (!excluded('plugins')) await loadPlugins() }
        catch (error) { console.error(error) }
        performance.mark('boot:plugins-ready')
        if (!isTauri && !excluded('account') && getDatabase().account) {
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
        LoadingStatusState.startedAt = null
        loadedStore.set(true)
        if (isTauri && !excluded('account') && nativeOfficialFlow) {
            void import('./storage/sync/nativeOfficialAccountOperations').then(module => module.recoverNativeOfficialAccountAssets())
                .catch(error => console.error('Official asset recovery could not start', error))
        }
        if (isTauri && recoveredNativeFileJobs.pendingSnapshotBodies?.length) {
            void import('./storage/nativePersistentMaintenance').then(module =>
                module.reattachNativeSnapshotRestoreBodies(recoveredNativeFileJobs.pendingSnapshotBodies),
            ).catch(error => console.error('Snapshot body reattachment failed', error))
        }
        setTimeout(() => {
            void runtime.expirePersistentTrash().catch((error) => console.error('Trash expiry failed', error))
                .then(() => forgetDisplayRecovery((target) =>
                    !getDatabase().characters.some((character) => character.chaId === target.characterId)))
        }, 0)
        void finishBoot()
        performance.mark('boot:interactive')
        if (!excluded('account')) {
            void nativePublicationRecovery?.reconcile(true).catch(error => {
                console.error('Official publication recovery failed', error)
            })
        }
        if (officialReconcilePublish && accountBootstrap.officialEnabled) {
            publishCurrentOfficialRevision().catch((error) => {
                console.error('Official reconcile publish failed', error)
            })
        }
        selectedCharID.set(-1)
        await yieldToUi()
        startObserveDom()
        registerModelDynamic()
        await saveDb()
        registerAndroidRisuSaveRoute()
        await reportInterruptedIOSBackupSourcesAtStart()
        if (recoveredNativeFileJobs.interruptedRestores.length > 0) {
            alertNormal(language.risuNest.backup.restoreInterrupted)
            await waitAlert()
        }
        if (isTauriAndroid || isTauriIOS) setGenerationBackground(beginGenerationBackground)
        if (isTauri) {
          schedulePeriodicNativeSnapshot();
          installIOSPersistenceLifecycle((reason) => flushLifecycle(reason, true), {
            settled: mobileDeferredHoldsSettled,
            expire: expireMobileDeferredHolds,
          });
        }
        if (!excluded('modules')) moduleUpdate()
        void alertTOS().then(async (accepted) => {
            if (accepted === false) {
                location.reload()
                return
            }
            if (isTauri && !pluginsExcludedAtStart) {
                void import('./plugins/defaultPluginBootstrap.svelte')
                    .then(({ startDefaultPluginBootstrap }) => startDefaultPluginBootstrap())
                    .catch(error => console.error('Default plugin bootstrap failed', error))
            }
            if (isTauri) {
                await showStoreCompactionNotice(storeCompaction)
                    .catch(error => console.error('Store compaction notice failed', error))
            }
            if (isTauri && !isTauriAndroid && await takeRendererRecovery()) {
                // A dialog replaces the toast, so it waits for queued dialogs.
                await waitAlert()
                alertToast(language.risuNest.startup.rendererRecovered)
            }
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
let disposeRuntimeErrors: (() => void) | undefined
function updateErrorHandling() {
    disposeRuntimeErrors ??= registerRuntimeErrorHandlers(
        window,
        alertError,
        isTauri ? recordNativeLogError : undefined,
    )
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

    if (isTauri) {
        // Asset objects are reclaimed by the native repository, which owns the
        // job pins and GC roots the content-addressed store is keyed on.
        const remotesDirectory = await nativeDataPath('remotes')
        if(!await exists(remotesDirectory)) {
            await mkdir(remotesDirectory, { recursive: true })
        }

        const remotes = await readDir(remotesDirectory)

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

                    const metaPath = await join(remotesDirectory, remote.name + '.meta')
                    let metaExists = false
                    let metaLastUsed:unknown
                    try {
                        metaExists = await exists(metaPath)
                        if (metaExists) {
                            const meta = await readFile(metaPath)
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
                        await writeFile(metaPath, new TextEncoder().encode(JSON.stringify(metaJson)))
                    }
                    else if(cleanupAction === 'delete'){
                        await remove(await join(remotesDirectory, remote.name))
                        await remove(metaPath)
                    }
                }
            } catch (error) {
                console.warn('Remote asset cleanup failed:', remote.name, error)
            }
        }
    }
    else {
        const uncleanable = new Set(await getUncleanables(db))
        const blobStore = await resolveBlobStore()
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
