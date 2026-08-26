import { language } from 'src/lang'

import { alertConfirm, alertError, alertNormal } from '../alert'
import { isTauriMobile } from '../platform'
import { loadPluginsAfterAuthoritativeRestore } from '../plugins/plugins.svelte'
import {
    listenAndroidSpoolBatches,
    type AndroidSpoolFailure,
    type AndroidSpoolReady,
} from './androidSafBridge'
import { createAndroidRisuSaveSpoolRoute } from './androidRisuSaveRoute'
import { runSharedNativeFileOperation } from './nativeFileJobManager'
import {
    NativeFileJobActivationCommittedError,
    NativeFileJobError,
    runNativeBlockRisuSaveRestore,
} from './nativeFileJobs'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'

let disposeSpoolListener: (() => void) | undefined

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

export function registerAndroidRisuSaveRoute(): void {
    if (!isTauriMobile || disposeSpoolListener) return

    const route = createAndroidRisuSaveSpoolRoute({
        confirmRestore: async () =>
            await alertConfirm(language.risuSaveImportConfirm)
            && await alertConfirm(language.backupLoadConfirm2),
        restore: async ({ source }) => {
            const result = await runSharedNativeFileOperation(
                'import',
                ({ signal, onStatus, setBlocking }) => runNativeBlockRisuSaveRestore(
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
    disposeSpoolListener = listenAndroidSpoolBatches((batch) => {
        void route.enqueue(batch)
    })
}
