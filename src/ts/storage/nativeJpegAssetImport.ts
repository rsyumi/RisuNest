import { invoke } from '@tauri-apps/api/core'

import {
    acquireDestructiveReplacementFence,
    capturePersistentMutationToken,
} from './persistentDataRuntime.svelte'
import {
    isTerminalJob as isTerminal,
    NativeFileJobActivationCommittedError,
    NativeFileJobError,
    type NativeFileJobOptions,
    type NativeFileJobSource,
    type NativeFileJobStatus,
} from './nativeFileJobs'
import type { PersistentMutationToken } from './saveCoordinator'
import type { PersistentDestructiveReplacementFence } from './persistentDataRuntime'

export interface NativeJpegAssetDestination {
    kind: 'current-character-image'
    characterId: string
}

export interface NativeJpegAssetImportInput {
    source: NativeFileJobSource
    displayName: string
    destination: NativeJpegAssetDestination
}

export interface NativeJpegAssetImportResult {
    revision: number
    logicalId: string
}

export interface NativeJpegAssetImportDependencies {
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    wait(milliseconds: number): Promise<void>
    captureMutationToken(reason: string): Promise<PersistentMutationToken>
    acquireFence(token: PersistentMutationToken): Promise<PersistentDestructiveReplacementFence>
}

const productionDependencies: NativeJpegAssetImportDependencies = {
    invoke,
    wait: (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds)),
    captureMutationToken: capturePersistentMutationToken,
    acquireFence: acquireDestructiveReplacementFence,
}

function abortError(): DOMException {
    return new DOMException('Native JPEG asset import was cancelled', 'AbortError')
}

function jpegExtension(displayName: string): 'jpg' | 'jpeg' {
    const extension = displayName.split('.').at(-1)?.toLocaleLowerCase('en-US')
    if (extension !== 'jpg' && extension !== 'jpeg') {
        throw new TypeError('Native JPEG asset import requires a .jpg or .jpeg display name')
    }
    return extension
}

export async function importNativeJpegAsset(
    input: NativeJpegAssetImportInput,
    options: NativeFileJobOptions = {},
    dependencies: NativeJpegAssetImportDependencies = productionDependencies,
): Promise<NativeJpegAssetImportResult> {
    const extension = jpegExtension(input.displayName)
    if (!input.destination.characterId) {
        throw new TypeError('Native JPEG asset import requires a destination character')
    }
    if (options.signal?.aborted) throw abortError()

    const token = await dependencies.captureMutationToken('native-jpeg-asset-import')
    const fence = await dependencies.acquireFence(token)
    let jobId: string | undefined
    let cancellationRequested = false
    let retainJobForRecovery = false
    try {
        if (options.signal?.aborted) throw abortError()
        const started = await dependencies.invoke('native_file_job_start', {
            request: {
                kind: 'import-jpeg-asset',
                source: input.source,
                displayName: input.displayName,
                destination: input.destination,
                expectedRevision: token.revision,
            },
        }) as { jobId: string }
        jobId = started.jobId

        let terminal: NativeFileJobStatus
        while (true) {
            if (options.signal?.aborted && !cancellationRequested) {
                cancellationRequested = true
                await dependencies.invoke('native_file_job_cancel', { jobId })
            }
            const status = await dependencies.invoke('native_file_job_status', {
                jobId,
            }) as NativeFileJobStatus
            options.onStatus?.(status)
            if (isTerminal(status)) {
                terminal = status
                break
            }
            await dependencies.wait(options.pollIntervalMs ?? 100)
        }

        if (terminal.state === 'cancelled') throw abortError()
        if (terminal.state !== 'succeeded') {
            throw new NativeFileJobError(
                terminal.error?.code ?? 'jpeg-asset-import-failed',
                terminal.error?.message ?? 'Native JPEG asset import failed',
            )
        }
        if (!terminal.result) {
            throw new NativeFileJobError(
                'missing-result',
                'Native JPEG asset import returned no result',
            )
        }
        const hash = terminal.result.sourceSha256
        if (!/^[0-9a-f]{64}$/.test(hash)) {
            throw new NativeFileJobError(
                'invalid-result',
                'Native JPEG asset import returned an invalid payload hash',
            )
        }
        try {
            await fence.refreshCommittedWorkingSet(terminal.result.revision)
        }
        catch (error) {
            retainJobForRecovery = true
            throw new NativeFileJobActivationCommittedError(terminal.result.revision, error)
        }
        return {
            revision: terminal.result.revision,
            logicalId: `assets/${hash}.${extension}`,
        }
    }
    finally {
        if (jobId && !retainJobForRecovery) {
            try {
                await dependencies.invoke('native_file_job_forget', { jobId })
            }
            catch {}
        }
        fence.release()
    }
}
