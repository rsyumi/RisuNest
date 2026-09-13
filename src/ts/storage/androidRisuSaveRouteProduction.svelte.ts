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
import { runExternalAndroidNativeFileOperation } from './nativeFileJobManager'
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

let disposeSpoolListener: (() => void) | undefined

export interface AndroidOpenedSpoolDispatchDependencies {
    enqueueRestore(batch: AndroidSpoolBatch): Promise<void>
    importCharacter(source: AndroidSpoolReady): Promise<NativeAndroidCharacterSpoolResult<string>>
    reportCharacterError(source: AndroidSpoolReady, error: unknown): void
    reportDestinationRequired(source: AndroidSpoolReady): void
}


export async function dispatchAndroidOpenedSpoolBatch(
    batch: AndroidSpoolBatch,
    dependencies: AndroidOpenedSpoolDispatchDependencies,
    handledCharacterTokens?: Set<string>,
): Promise<void> {
    const characterSources = batch.ready.filter(isAndroidNativeContentSpool)
    await dependencies.enqueueRestore({
        ...batch,
        ready: batch.ready.filter((source) => !isAndroidNativeContentSpool(source)),
    })
    for (const source of characterSources) {
        if (handledCharacterTokens?.has(source.token)) continue
        handledCharacterTokens?.add(source.token)
        try {
            const result = await dependencies.importCharacter(source)
            if (result.kind === 'destination-required') {
                dependencies.reportDestinationRequired(source)
            }
        }
        catch (error) {
            dependencies.reportCharacterError(source, error)
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

async function importAndroidCharacterSpool(
    source: AndroidSpoolReady,
): Promise<NativeAndroidCharacterSpoolResult<string>> {
    const [
        { importAndroidNativeCharacterSpool },
        { isNativeCharacterContentImportEnabled },
        { activatePreparedNativeCharacterContent },
        { activatePreparedNativeModuleContent },
    ] = await Promise.all([
        import('./nativeCharacterFileRoute'),
        import('../characterCards'),
        import('./nativeCharacterContentActivation'),
        import('./nativeModuleContentActivation'),
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
                    importAndroidOpenedPreparedContent(
                        source,
                        {
                            prepare: prepareNativeContentImport,
                            activateCharacter: (
                                content,
                                lifecycle,
                                activationSignal,
                            ) =>
                                activatePreparedNativeCharacterContent(
                                    content,
                                    lifecycle,
                                    undefined,
                                    activationSignal,
                                ),
                            activateModule: (
                                content,
                                lifecycle,
                                activationSignal,
                            ) =>
                                activatePreparedNativeModuleContent(
                                    content,
                                    lifecycle,
                                    undefined,
                                    activationSignal,
                                ),
                        },
                        { signal, onStatus },
                    ),
                { presentation: 'dialog', format: 'content' },
            ),
        legacyImport: async () => {
            throw new Error(
                'Android spool character import has no legacy byte fallback',
            )
        },
    })
}

function showRestoreError(error: unknown): void {
    if (error instanceof DOMException && error.name === 'AbortError') return
    if (error instanceof NativeFileJobActivationCommittedError) {
        alertError(language.risuSaveImportCommittedRefreshFailed)
        return
    }
    if (error instanceof NativeFileJobError && error.code === 'revision-conflict') {
        alertError(language.risuSaveRevisionConflict)
        return
    }
    alertError(error instanceof Error ? error.message : String(error))
}

function showSpoolFailure(failure: AndroidSpoolFailure): void {
    alertError(`${failure.displayName}: ${failure.code}`)
}

function showUnsupportedSpool(source: AndroidSpoolReady): void {
    alertError(`${source.displayName}: unsupported-format`)
}

function showDestinationRequired(source: AndroidSpoolReady): void {
    alertError(`${source.displayName}: destination-required`)
}

/** The common restore entry owns confirmation, the shared operation, and source cleanup. */
export async function restoreAndroidOpenedBackupSource({
    source,
    displayName,
}: AndroidRisuSaveRestoreInput): Promise<void> {
    const { restoreBackupFromNativeSource } =
        await import('./portableBackupFileRouteProduction.svelte')
    const result = await restoreBackupFromNativeSource(async ({ onSource }) => {
        onSource({ name: displayName })
        return source
    })
    if (!result) return
    alertNormal(
        result.warningCodes.includes('cleanup-failed')
            ? language.risuSaveCleanupWarning
            : language.risuSaveImportComplete,
    )
}

export function registerAndroidRisuSaveRoute(): void {
    if (!isTauriAndroid || !isAndroidSafFileJobsEnabled() || disposeSpoolListener) return

    listenRecoveredAndroidRisuSavePublications(
        (terminal) => {
            if (terminal.warningCodes.includes('partial-destination-may-remain')) {
                alertError(language.screenshotPartialDestinationMayRemain)
            }
        },
        (error) => alertError(error instanceof Error ? error.message : String(error)),
        {
            getStatus: getAndroidSafExportStatus,
            acknowledge: acknowledgeAndroidSafExport,
            listen: listenAndroidSafDestinationEvents,
            isActive: isAndroidSafDestinationRequestActive,
        },
    )

    const route = createAndroidRisuSaveSpoolRoute({
        confirmRestore: async () => true,
        discard: (source) => {
            if (!discardAndroidSafSource(source.token)) {
                alertError(`${source.displayName}: discard-failed`)
            }
        },
        restore: restoreAndroidOpenedBackupSource,
        unsupported: showUnsupportedSpool,
        failed: showSpoolFailure,
        onError: (_source, error) => showRestoreError(error),
    })
    const dispatcher = createAndroidOpenedSpoolDispatcher({
        enqueueRestore: route.enqueue,
        importCharacter: importAndroidCharacterSpool,
        reportCharacterError: (_source, error) => showRestoreError(error),
        reportDestinationRequired: showDestinationRequired,
    })
    disposeSpoolListener = listenAndroidSpoolBatches((batch) => {
        void dispatcher.enqueue(batch)
    })
}
