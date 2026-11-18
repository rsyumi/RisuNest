import { get } from 'svelte/store'
import { onboardingHold } from 'src/lib/Others/Onboarding/onboardingGate'
import { failureReason } from '../gui/nativeFileJobDialogModel'
import { fileOperationErrorWasPresented } from './fileOperationErrorPresentation'
import { Mutex } from '../mutex'
import { language } from 'src/lang'

import { alertError, alertNormal } from '../alert'
import { isTauriAndroid } from '../platform'
import {
    acknowledgeAndroidSafExport,
    discardAndroidSafSource,
    getAndroidSafExportStatus,
    isAndroidSafDestinationRequestActive,
    isAndroidSafFileJobsEnabled,
    listenAndroidSafDestinationEvents,
    listenAndroidSpoolBatches,
    type AndroidSpoolBatch,
    type AndroidSpoolFailure,
    isAndroidNativeContentSpool,
    type AndroidSpoolReady,
} from './androidSafBridge'
import {
    createAndroidRisuSaveSpoolRoute,
    type AndroidRisuSaveRestoreInput,
} from './androidRisuSaveRoute'
import { runExternalAndroidNativeFileOperation, waitForNativeFileOperationIdle } from './nativeFileJobManager'
import type { NativeAndroidCharacterSpoolResult } from './nativeCharacterFileRoute'
import {
    NativeFileJobActivationCommittedError,
    NativeFileJobError,
    prepareNativeContentImport,
    type NativeFileJobOptions,
    type NativeFileJobSource,
    type PreparedNativeContent,
    type PreparedNativeContentReceipt,
} from './nativeFileJobs'
import { runNativePreparedContentRoute } from './nativePreparedContentRoute'
import { listenRecoveredAndroidRisuSavePublications } from './risuSaveFileRoute'
import { createPortableExportIntentStore } from './deviceBackup/job'
import { handOverOpenedFiles, openedFileSource, settleOpenedFilesAfter } from '../openedFiles'
import type { ImportIntakeDelivery, ImportIntakeSource } from '../importIntake/types'

let disposeSpoolListener: (() => void) | undefined

/** Whether first setup is finished and the onboarding is gone. A restore during onboarding sets `didFirstSetup` while the onboarding stays. */
export async function androidFirstSetupFinished(): Promise<boolean> {
    return (await import('./database.svelte')).getDatabase().didFirstSetup && !get(onboardingHold)
}

export interface AndroidSpoolBatchRouting {
    /** Spools that arrive before first setup keep their own handling. */
    firstSetupDone(): Promise<boolean>
    loadIntake(): Promise<(delivery: ImportIntakeDelivery) => Promise<void>>
    /** The handling before first setup. */
    dispatch(batch: AndroidSpoolBatch): Promise<void>
}

/**
 * Hands each opened or shared spool batch to the import intake as one delivery, in arrival order,
 * together with the opened files of the same intent that Android copied instead of spooling.
 * A content picker result that outlived its page keeps the destination chosen in the picker.
 */
export function createAndroidSpoolBatchRouter(
    routing: AndroidSpoolBatchRouting,
): (batch: AndroidSpoolBatch) => Promise<void> {
    const submittedTokens = new Set<string>()
    let queue = Promise.resolve()
    return ({ openedFiles = [], order, ...batch }) => {
        const routed = queue.then(async () => {
            if (!await routing.firstSetupDone()) {
                // These files keep the handling they had as their own opened-files event.
                if (openedFiles.length) handOverOpenedFiles(openedFiles)
                void routing.dispatch(batch)
                return
            }
            let delivery: Promise<void> | undefined
            try {
                const submit = await routing.loadIntake()
                const fresh = batch.ready.filter((source) => !submittedTokens.has(source.token))
                for (const source of fresh) submittedTokens.add(source.token)
                const picked = fresh.filter((source) => source.importDestination)
                // A content picker result arrives as a batch of its own, so its failures stay with it.
                const pickerFailures = batch.ready.some((source) => source.importDestination) ? batch.failures : []
                if (picked.length) void routing.dispatch({ ...batch, ready: picked, failures: pickerFailures })
                const sources: ImportIntakeSource[] = [
                    ...fresh
                        .filter((source) => !source.importDestination)
                        .map((spool) => ({ type: 'androidSpool' as const, spool })),
                    ...openedFiles.map(openedFileSource),
                    ...batch.failures
                        .filter((failure) => !pickerFailures.includes(failure))
                        .map(({ displayName, code }) => ({ type: 'failed' as const, name: displayName, code })),
                ]
                if (sources.length) delivery = submit({ origin: 'opened', sources: inOpeningOrder(sources, order) })
            } finally {
                // Settled as the opened-files handling settles them, once this delivery has ended.
                if (openedFiles.length) void settleOpenedFilesAfter(openedFiles, async () => await delivery)
            }
        })
        queue = routed.catch(() => {})
        return routed
    }
}

/**
 * Puts the sources in the order Android opened them. Batches that reached the page together carry
 * their orders one after another, so a repeated name takes the next position it has.
 */
function inOpeningOrder(sources: ImportIntakeSource[], order: string[] | undefined): ImportIntakeSource[] {
    if (!order?.length) return sources
    const positions = new Map<string, number[]>()
    order.forEach((key, position) => positions.set(key, [...positions.get(key) ?? [], position]))
    const key = (source: ImportIntakeSource) => source.type === 'androidSpool' ? source.spool.token
        : source.type === 'path' ? source.path : source.type === 'failed' ? source.name : ''
    return sources
        .map(source => ({ source, position: positions.get(key(source))?.shift() ?? Infinity }))
        .sort((a, b) => a.position - b.position)
        .map(({ source }) => source)
}

export interface AndroidOpenedSpoolDispatchDependencies {
    enqueueRestore(batch: AndroidSpoolBatch): Promise<void>
    importCharacter(source: AndroidSpoolReady): Promise<NativeAndroidCharacterSpoolResult<string>>
    /** Imports two or more content files, with the ones Android could not copy, in one operation. */
    importContentBatch(sources: AndroidSpoolReady[], failures: AndroidSpoolFailure[]): Promise<unknown>
    reportCharacterError(error: unknown, startedAt: number): void
    reportDestinationRequired(source: AndroidSpoolReady, startedAt: number): void
}


export async function dispatchAndroidOpenedSpoolBatch(
    batch: AndroidSpoolBatch,
    dependencies: AndroidOpenedSpoolDispatchDependencies,
    handledCharacterTokens?: Set<string>,
): Promise<void> {
    const contentSources = batch.ready.filter((source) =>
        isAndroidNativeContentSpool(source) && !handledCharacterTokens?.has(source.token))
    const contentFailures = batch.failures.filter(isAndroidNativeContentSpool)
    const batched = contentSources.length + contentFailures.length > 1
    // Backups keep their own restore, which runs before the content files.
    await dependencies.enqueueRestore({
        ...batch,
        ready: batch.ready.filter((source) => !isAndroidNativeContentSpool(source)),
        failures: batched
            ? batch.failures.filter((failure) => !isAndroidNativeContentSpool(failure))
            : batch.failures,
    })
    for (const source of contentSources) handledCharacterTokens?.add(source.token)
    if (batched) {
        const startedAt = Date.now()
        try {
            await dependencies.importContentBatch(contentSources, contentFailures)
        }
        catch (error) {
            dependencies.reportCharacterError(error, startedAt)
        }
        return
    }
    for (const source of contentSources) {
        const startedAt = Date.now()
        try {
            const result = await dependencies.importCharacter(source)
            if (result.kind === 'destination-required') {
                dependencies.reportDestinationRequired(source, startedAt)
            }
        }
        catch (error) {
            dependencies.reportCharacterError(error, startedAt)
        }
    }
}

export function createAndroidOpenedSpoolDispatcher(
    dependencies: AndroidOpenedSpoolDispatchDependencies,
): { enqueue(batch: AndroidSpoolBatch): Promise<void> } {
    const handledCharacterTokens = new Set<string>()
    const dispatchMutex = new Mutex()
    return {
        enqueue(batch) {
            return dispatchMutex.runExclusive(() => dispatchAndroidOpenedSpoolBatch(
                batch,
                dependencies,
                handledCharacterTokens,
            ))
        },
    }
}

export interface AndroidOpenedPreparedContentDependencies {
    prepare(
        source: NativeFileJobSource,
        displayName: string,
        options?: NativeFileJobOptions,
    ): Promise<PreparedNativeContentReceipt>
    activateCharacter(
        content: PreparedNativeContent,
        lifecycle: PreparedNativeContentReceipt,
        signal?: AbortSignal,
    ): Promise<{ characterId: string } | null>
    activateModule(
        content: PreparedNativeContent,
        lifecycle: PreparedNativeContentReceipt,
        signal?: AbortSignal,
    ): Promise<{ moduleId: string } | null>
}

export async function importAndroidOpenedPreparedContent(
    source: AndroidSpoolReady,
    dependencies: AndroidOpenedPreparedContentDependencies,
    options: NativeFileJobOptions = {},
): Promise<{ kind: 'declined' } | { kind: 'imported'; value: string }> {
    const result = await runNativePreparedContentRoute(
        { type: 'androidSpool', token: source.token },
        source.displayName,
        {
            prepare: dependencies.prepare,
            map: async (content) => content,
            activate: async (content, lifecycle, signal) => {
                if (content.format === 'risu-module') {
                    const activated = await dependencies.activateModule(content, lifecycle, signal)
                    return activated ? activated.moduleId : null
                }
                const activated = await dependencies.activateCharacter(content, lifecycle, signal)
                return activated ? activated.characterId : null
            },
        },
        options,
    )
    return result === null
        ? { kind: 'declined' }
        : { kind: 'imported', value: result }
}

async function openedPreparedContentDependencies(): Promise<AndroidOpenedPreparedContentDependencies> {
    const [
        { activatePreparedNativeCharacterContent },
        { activatePreparedNativeModuleContent },
    ] = await Promise.all([
        import('./nativeCharacterContentActivation'),
        import('./nativeModuleContentActivation'),
    ])
    return {
        prepare: prepareNativeContentImport,
        activateCharacter: (content, lifecycle, signal) =>
            activatePreparedNativeCharacterContent(content, lifecycle, undefined, signal),
        activateModule: (content, lifecycle, signal) =>
            activatePreparedNativeModuleContent(content, lifecycle, undefined, signal),
    }
}

export async function importAndroidContentSpoolBatch(
    sources: AndroidSpoolReady[],
    failures: AndroidSpoolFailure[],
): Promise<string | null> {
    const [{ importOpenedAndroidContentSpools }, dependencies] = await Promise.all([
        import('./androidContentPicker'),
        openedPreparedContentDependencies(),
    ])
    return await importOpenedAndroidContentSpools(sources, failures, async (source, context) => {
        const result = await importAndroidOpenedPreparedContent(
            source,
            dependencies,
            { signal: context.signal, onStatus: context.onStatus },
        )
        return result.kind === 'imported' ? result.value : null
    })
}

export async function importAndroidCharacterSpool(
    source: AndroidSpoolReady,
): Promise<NativeAndroidCharacterSpoolResult<string>> {
    const { androidSpoolReplayDestination, importReplayedAndroidContentSpool } = await import(
        './androidContentPicker'
    )
    const replayDestination = androidSpoolReplayDestination(source)
    if (replayDestination) {
        const value = await importReplayedAndroidContentSpool(
            source,
            replayDestination,
        )
        return value === null
            ? { kind: 'declined' }
            : { kind: 'imported', value }
    }
    const [
        { importAndroidNativeCharacterSpool },
        { isNativeCharacterContentImportEnabled },
        dependencies,
    ] = await Promise.all([
        import('./nativeCharacterFileRoute'),
        import('../characterCards'),
        openedPreparedContentDependencies(),
    ])
    return await importAndroidNativeCharacterSpool(source, {
        chooseDesktopPath: async () => null,
        readDesktopPath: async () => {
            throw new Error(
                'Android spool character import cannot read source bytes in TypeScript',
            )
        },
        nativeEnabled: isNativeCharacterContentImportEnabled,
        nativeImport: async () =>
            await runExternalAndroidNativeFileOperation(
                'import',
                ({ signal, onStatus }) =>
                    importAndroidOpenedPreparedContent(source, dependencies, { signal, onStatus }),
                { presentation: 'dialog', format: 'content' },
            ),
        legacyImport: async () => {
            throw new Error(
                'Android spool character import has no legacy byte fallback',
            )
        },
    })
}

export function showAndroidFileError(error: unknown, startedAt = Infinity): void {
    if (fileOperationErrorWasPresented('import', startedAt)) return
    if (error instanceof DOMException && error.name === 'AbortError') return
    if (error instanceof NativeFileJobActivationCommittedError) {
        alertError(language.risuSaveImportCommittedRefreshFailed)
        return
    }
    if (error instanceof NativeFileJobError && error.code === 'revision-conflict') {
        alertError(language.risuSaveRevisionConflict)
        return
    }
    const code = error && typeof error === 'object' && 'code' in error && typeof error.code === 'string'
        ? error.code : ''
    alertError(failureReason(code))
}

function showSpoolFailure(failure: AndroidSpoolFailure): void {
    alertError(`${failure.displayName}: ${failureReason(failure.code)}`)
}

function showUnsupportedSpool(source: AndroidSpoolReady): void {
    alertError(`${source.displayName}: ${failureReason('unsupported-format')}`)
}

export function showDestinationRequired(source: AndroidSpoolReady, startedAt: number): void {
    if (fileOperationErrorWasPresented('import', startedAt)) return
    alertError(`${source.displayName}: ${failureReason('destination-required')}`)
}

/** The common restore entry owns confirmation, the shared operation, and source cleanup. */
export async function restoreAndroidOpenedBackupSource({
    source,
    displayName,
    bytes,
}: AndroidRisuSaveRestoreInput): Promise<void> {
    const { restoreBackupFromNativeSource } =
        await import('./portableBackupFileRouteProduction.svelte')
    const result = await restoreBackupFromNativeSource(async ({ onSource }) => {
        onSource({ name: displayName, bytes })
        return source
    })
    if (!result) return
    alertNormal(
        result.warningCodes.includes('cleanup-failed')
            ? language.risuSaveCleanupWarning
            : language.risuSaveImportComplete,
    )
}

/** Restores a backup shared before first setup and releases its spool if the restore never took it. */
export async function restoreAndroidBackupSpool(input: AndroidRisuSaveRestoreInput): Promise<void> {
    const startedAt = Date.now()
    try {
        await restoreAndroidOpenedBackupSource(input)
    } catch (error) {
        showAndroidFileError(error, startedAt)
    }
    // A restore can read its spool until its outcome and any retry from it are gone.
    await waitForNativeFileOperationIdle()
    if (input.source.type === 'androidSpool') await discardAndroidSafSource(input.source.token).catch(() => false)
}

export function registerAndroidRisuSaveRoute(): void {
    if (!isTauriAndroid || !isAndroidSafFileJobsEnabled() || disposeSpoolListener) return

    listenRecoveredAndroidRisuSavePublications(
        (terminal) => {
            if (terminal.warningCodes.includes('partial-destination-may-remain')) {
                alertError(language.screenshotPartialDestinationMayRemain)
            }
        },
        (error) => showAndroidFileError(error),
        {
            getStatus: getAndroidSafExportStatus,
            acknowledge: acknowledgeAndroidSafExport,
            listen: listenAndroidSafDestinationEvents,
            isActive: (requestId) => {
                if (isAndroidSafDestinationRequestActive(requestId)) return true
                try {
                    const intent = createPortableExportIntentStore().read()
                    return intent?.publication === 'android-saf' && intent.requestId === requestId
                } catch {
                    return false
                }
            },
        },
    )

    const route = createAndroidRisuSaveSpoolRoute({
        confirmRestore: async () => true,
        discard: async (source) => {
            if (!await discardAndroidSafSource(source.token)) {
                alertError(`${source.displayName}: ${failureReason('discard-failed')}`)
            }
        },
        restore: restoreAndroidBackupSpool,
        unsupported: showUnsupportedSpool,
        failed: showSpoolFailure,
        onError: (_source, error) => showAndroidFileError(error),
    })
    const dispatcher = createAndroidOpenedSpoolDispatcher({
        enqueueRestore: route.enqueue,
        importCharacter: importAndroidCharacterSpool,
        importContentBatch: importAndroidContentSpoolBatch,
        reportCharacterError: showAndroidFileError,
        reportDestinationRequired: showDestinationRequired,
    })
    const routeSpoolBatch = createAndroidSpoolBatchRouter({
        firstSetupDone: androidFirstSetupFinished,
        loadIntake: async () => (await import('../importIntake/intake')).submitImportDelivery,
        dispatch: dispatcher.enqueue,
    })
    disposeSpoolListener = listenAndroidSpoolBatches((batch) => {
        void routeSpoolBatch(batch).catch((error) => showAndroidFileError(error))
    })
}
