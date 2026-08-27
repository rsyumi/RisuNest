import { language } from 'src/lang'

import { alertConfirm, alertError, alertNormal } from '../alert'
import { isTauriAndroid } from '../platform'
import { loadPluginsAfterAuthoritativeRestore } from '../plugins/plugins.svelte'
import {
    discardAndroidSafSource,
    isAndroidSafFileJobsEnabled,
    listenAndroidSpoolBatches,
    type AndroidSpoolBatch,
    type AndroidSpoolFailure,
    type AndroidSpoolReady,
} from './androidSafBridge'
import { createAndroidRisuSaveSpoolRoute } from './androidRisuSaveRoute'
import { runExternalAndroidNativeFileOperation } from './nativeFileJobManager'
import type { NativeAndroidCharacterSpoolResult } from './nativeCharacterFileRoute'
import {
    NativeFileJobActivationCommittedError,
    NativeFileJobError,
    runNativeBlockRisuSaveRestore,
    runNativeLosslessBackupRestore,
} from './nativeFileJobs'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'

let disposeSpoolListener: (() => void) | undefined

export interface AndroidOpenedSpoolDispatchDependencies {
    enqueueRestore(batch: AndroidSpoolBatch): Promise<void>
    importCharacter(source: AndroidSpoolReady): Promise<NativeAndroidCharacterSpoolResult<string>>
    reportCharacterError(source: AndroidSpoolReady, error: unknown): void
    reportDestinationRequired(source: AndroidSpoolReady): void
}

function isAndroidNativeCharacterSpool(source: AndroidSpoolReady): boolean {
    const displayName = source.displayName.toLocaleLowerCase('en-US')
    return displayName.endsWith('.json')
        || displayName.endsWith('.charx')
        || displayName.endsWith('.jpg')
        || displayName.endsWith('.jpeg')
}

export async function dispatchAndroidOpenedSpoolBatch(
    batch: AndroidSpoolBatch,
    dependencies: AndroidOpenedSpoolDispatchDependencies,
    handledCharacterTokens?: Set<string>,
): Promise<void> {
    const characterSources = batch.ready.filter(isAndroidNativeCharacterSpool)
    await dependencies.enqueueRestore({
        ...batch,
        ready: batch.ready.filter((source) => !isAndroidNativeCharacterSpool(source)),
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
    let tail: Promise<void> = Promise.resolve()
    return {
        enqueue(batch) {
            const queued = tail.then(async () => {
                await dispatchAndroidOpenedSpoolBatch(
                    batch,
                    dependencies,
                    handledCharacterTokens,
                )
            })
            tail = queued.then(
                () => undefined,
                () => undefined,
            )
            return queued
        },
    }
}

async function importAndroidCharacterSpool(
    source: AndroidSpoolReady,
): Promise<NativeAndroidCharacterSpoolResult<string>> {
    const [
        { importAndroidNativeCharacterSpool },
        {
            importPreparedNativeCharacterContent,
            isNativeCharacterContentImportEnabled,
        },
    ] = await Promise.all([
        import('./nativeCharacterFileRoute'),
        import('../characterCards'),
    ])
    return await importAndroidNativeCharacterSpool(source, {
        chooseDesktopPath: async () => null,
        readDesktopPath: async () => {
            throw new Error('Android spool character import cannot read source bytes in TypeScript')
        },
        nativeEnabled: isNativeCharacterContentImportEnabled,
        nativeImport: async (input) => await runExternalAndroidNativeFileOperation(
            'import',
            ({ signal, onStatus }) => importPreparedNativeCharacterContent(input, {
                signal,
                onStatus,
            }),
        ),
        legacyImport: async () => {
            throw new Error('Android spool character import has no legacy byte fallback')
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

export function registerAndroidRisuSaveRoute(): void {
    if (!isTauriAndroid || !isAndroidSafFileJobsEnabled() || disposeSpoolListener) return

    const route = createAndroidRisuSaveSpoolRoute({
        confirmRestore: async () =>
            await alertConfirm(language.risuSaveImportConfirm)
            && await alertConfirm(language.backupLoadConfirm2),
        discard: (source) => {
            if (!discardAndroidSafSource(source.token)) {
                alertError(`${source.displayName}: discard-failed`)
            }
        },
        restore: async ({ source, displayName }) => {
            const lossless = displayName.toLocaleLowerCase('en-US').endsWith('.risulossless')
            const result = await runExternalAndroidNativeFileOperation(
                'import',
                ({ signal, onStatus, setBlocking }) => (lossless
                    ? runNativeLosslessBackupRestore
                    : runNativeBlockRisuSaveRestore)(
                    getPersistentDataRuntime(),
                    source,
                    {
                        signal,
                        onStatus,
                        onBlockingChange: setBlocking,
                        afterRefresh: loadPluginsAfterAuthoritativeRestore,
                    },
                ),
            )
            alertNormal(result.warningCodes.includes('cleanup-failed')
                ? language.risuSaveCleanupWarning
                : language.risuSaveImportComplete)
        },
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
