import { invoke } from '@tauri-apps/api/core'

import { isTauri } from '../platform'

export type NativeFileJobSource =
    | { type: 'desktopPath'; path: string }
    | { type: 'androidSpool'; token: string }

export type NativeFileJobState =
    | 'queued'
    | 'running'
    | 'waitingForInput'
    | 'cancelling'
    | 'succeeded'
    | 'failed'
    | 'cancelled'

export interface NativeFileJobResult {
    revision: number
    sourceBytes: number
    sourceSha256: string
    characterCount: number
    presetCount: number
    warningCodes: string[]
}

export interface NativeFileJobStatus {
    jobId: string
    kind: 'restore-block-risu-save'
    state: NativeFileJobState
    phase: 'queued' | 'reading-source' | 'staging-database' | 'activating-database' | 'complete'
    progress: {
        completedBytes: number
        totalBytes?: number
        completedItems: number
        totalItems?: number
    }
    result?: NativeFileJobResult
    error?: {
        code: string
        message: string
    }
}

export interface NativeBlockRestoreRuntime {
    readonly revision: number
    flushPendingData(reason: string): Promise<void>
    refreshActiveWorkingSet(revision: number): Promise<void>
}

export interface NativeFileJobOptions {
    signal?: AbortSignal
    pollIntervalMs?: number
}

export interface NativeFileJobDependencies {
    isTauri(): boolean
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    wait(milliseconds: number): Promise<void>
}

const productionDependencies: NativeFileJobDependencies = {
    isTauri: () => isTauri,
    invoke: (command, args) => args === undefined ? invoke(command) : invoke(command, args),
    wait: (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds)),
}

export class NativeFileJobError extends Error {
    constructor(readonly code: string, message: string) {
        super(message)
        this.name = 'NativeFileJobError'
    }
}

function abortError(): Error {
    return new DOMException('Native file job was cancelled', 'AbortError')
}

export async function runNativeBlockRisuSaveRestore(
    runtime: NativeBlockRestoreRuntime,
    source: NativeFileJobSource,
    options: NativeFileJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    if (!dependencies.isTauri()) {
        throw new Error('Native block RisuSave restore requires Tauri')
    }
    if (options.signal?.aborted) throw abortError()

    await runtime.flushPendingData('native-block-risu-save-restore')
    const expectedRevision = runtime.revision
    const started = await dependencies.invoke('native_file_job_start', {
        request: {
            kind: 'restore-block-risu-save',
            source,
            expectedRevision,
        },
    }) as { jobId: string }
    let cancellationRequested = false
    let terminal: NativeFileJobStatus | undefined

    while (!terminal) {
        if (options.signal?.aborted && !cancellationRequested) {
            cancellationRequested = true
            await dependencies.invoke('native_file_job_cancel', { jobId: started.jobId })
        }
        const status = await dependencies.invoke('native_file_job_status', {
            jobId: started.jobId,
        }) as NativeFileJobStatus
        if (status.state === 'succeeded' || status.state === 'failed' || status.state === 'cancelled') {
            terminal = status
            break
        }
        await dependencies.wait(options.pollIntervalMs ?? 100)
    }

    if (terminal.state === 'succeeded') {
        if (!terminal.result) {
            throw new NativeFileJobError('missing-result', 'Native restore returned no result')
        }
        await runtime.refreshActiveWorkingSet(terminal.result.revision)
        await dependencies.invoke('native_file_job_forget', { jobId: started.jobId })
        return terminal.result
    }

    await dependencies.invoke('native_file_job_forget', { jobId: started.jobId })
    if (terminal.state === 'cancelled') throw abortError()
    throw new NativeFileJobError(
        terminal.error?.code ?? 'restore-failed',
        terminal.error?.message ?? 'Native block RisuSave restore failed',
    )
}
