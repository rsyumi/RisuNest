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
    kind: 'restore-block-risu-save' | 'export-block-risu-save'
    expectedRevision?: number
    warningCodes?: string[]
    state: NativeFileJobState
    phase:
        | 'queued'
        | 'reading-source'
        | 'staging-database'
        | 'awaiting-activation'
        | 'activating-database'
        | 'writing-export'
        | 'publishing-destination'
        | 'finalizing-export'
        | 'complete'
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
    capturePersistentMutationToken(reason: string): Promise<{
        revision: number
        mutationGeneration: number
    }>
    acquireDestructiveReplacementFence(token: {
        revision: number
        mutationGeneration: number
    }): Promise<{
        refreshCommittedWorkingSet(revision: number): Promise<void>
        release(): void
    }>
}

export interface NativeFileJobOptions {
    signal?: AbortSignal
    pollIntervalMs?: number
    onStatus?(status: NativeFileJobStatus): void
}

export interface NativeFileRestoreJobOptions extends NativeFileJobOptions {
    afterRefresh?(): void | Promise<void>
    onBlockingChange?(blocking: boolean): void
}

export interface NativeFileExportJobOptions extends NativeFileJobOptions {
    omitAccount?: boolean
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

export class NativeFileJobActivationCommittedError extends NativeFileJobError {
    readonly recoveryRequired = true

    constructor(
        readonly committedRevision: number,
        readonly cause: unknown,
    ) {
        super(
            'activation-committed-refresh-failed',
            `Native restore committed revision ${committedRevision}, but the active app state could not be fully refreshed`,
        )
        this.name = 'NativeFileJobActivationCommittedError'
    }
}

function abortError(): Error {
    return new DOMException('Native file job was cancelled', 'AbortError')
}

async function invokeNative(
    dependencies: NativeFileJobDependencies,
    command: string,
    args?: Record<string, unknown>,
): Promise<unknown> {
    try {
        return await dependencies.invoke(command, args)
    }
    catch (error) {
        if (
            typeof error === 'object'
            && error !== null
            && 'code' in error
            && typeof error.code === 'string'
            && 'message' in error
            && typeof error.message === 'string'
        ) {
            throw new NativeFileJobError(error.code, error.message)
        }
        throw error
    }
}

export async function runNativeBlockRisuSaveRestore(
    runtime: NativeBlockRestoreRuntime,
    source: NativeFileJobSource,
    options: NativeFileRestoreJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    if (!dependencies.isTauri()) {
        throw new Error('Native block RisuSave restore requires Tauri')
    }
    if (options.signal?.aborted) throw abortError()

    const mutationToken = await runtime.capturePersistentMutationToken(
        'native-block-risu-save-restore',
    )
    if (options.signal?.aborted) throw abortError()
    const started = await invokeNative(dependencies, 'native_file_job_start', {
        request: {
            kind: 'restore-block-risu-save',
            source,
            expectedRevision: mutationToken.revision,
        },
    }) as { jobId: string; warningCodes?: string[] }
    let cancellationRequested = false
    let terminal: NativeFileJobStatus | undefined
    let mutationConflict: NativeFileJobError | undefined
    let replacementFence: Awaited<ReturnType<
        NativeBlockRestoreRuntime['acquireDestructiveReplacementFence']
    >> | undefined
    try {
        while (!terminal) {
            if (options.signal?.aborted && !cancellationRequested) {
                cancellationRequested = true
                await invokeNative(dependencies, 'native_file_job_cancel', {
                    jobId: started.jobId,
                })
            }
            const status = await invokeNative(dependencies, 'native_file_job_status', {
                jobId: started.jobId,
            }) as NativeFileJobStatus
            options.onStatus?.(status)
            if (
                status.state === 'waitingForInput'
                && status.phase === 'awaiting-activation'
                && !replacementFence
                && !cancellationRequested
            ) {
                try {
                    replacementFence = await runtime.acquireDestructiveReplacementFence(
                        mutationToken,
                    )
                }
                catch (error) {
                    mutationConflict = new NativeFileJobError(
                        'revision-conflict',
                        error instanceof Error ? error.message : String(error),
                    )
                    cancellationRequested = true
                    await invokeNative(dependencies, 'native_file_job_cancel', {
                        jobId: started.jobId,
                    })
                    continue
                }
                options.onBlockingChange?.(true)
                await invokeNative(dependencies, 'native_file_job_finalize', {
                    jobId: started.jobId,
                })
                options.onStatus?.({
                    ...status,
                    state: 'running',
                    phase: 'activating-database',
                })
            }
            if (
                status.state === 'succeeded'
                || status.state === 'failed'
                || status.state === 'cancelled'
            ) {
                terminal = status
                break
            }
            await dependencies.wait(options.pollIntervalMs ?? 100)
        }

        if (terminal.state === 'succeeded') {
            if (!terminal.result) {
                throw new NativeFileJobError('missing-result', 'Native restore returned no result')
            }
            if (!replacementFence) {
                throw new NativeFileJobError(
                    'missing-activation-fence',
                    'Native restore committed without a renderer replacement fence',
                )
            }
            try {
                await replacementFence.refreshCommittedWorkingSet(terminal.result.revision)
                await options.afterRefresh?.()
            }
            catch (error) {
                throw new NativeFileJobActivationCommittedError(terminal.result.revision, error)
            }
            const committedResult = {
                ...terminal.result,
                warningCodes: [...new Set([
                    ...(started.warningCodes ?? []),
                    ...terminal.result.warningCodes,
                ])].slice(0, 16),
            }
            try {
                await invokeNative(dependencies, 'native_file_job_forget', {
                    jobId: started.jobId,
                })
            }
            catch {
                committedResult.warningCodes = [
                    ...committedResult.warningCodes
                        .filter((code) => code !== 'cleanup-failed')
                        .slice(0, 15),
                    'cleanup-failed',
                ]
            }
            return committedResult
        }

        const error = mutationConflict ?? (terminal.state === 'cancelled'
            ? abortError()
            : new NativeFileJobError(
                terminal.error?.code ?? 'restore-failed',
                terminal.error?.message ?? 'Native block RisuSave restore failed',
            ))
        try {
            await invokeNative(dependencies, 'native_file_job_forget', {
                jobId: started.jobId,
            })
        }
        catch {}
        throw error
    }
    finally {
        replacementFence?.release()
        options.onBlockingChange?.(false)
    }
}

export async function runNativeBlockRisuSaveExport(
    runtime: {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    destination: string,
    options: NativeFileExportJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    if (!dependencies.isTauri()) {
        throw new Error('Native block RisuSave export requires Tauri')
    }
    if (options.signal?.aborted) throw abortError()

    await runtime.flushPendingData('native-block-risu-save-export')
    if (options.signal?.aborted) throw abortError()
    const expectedRevision = runtime.revision
    const started = await invokeNative(dependencies, 'native_file_job_start', {
        request: {
            kind: 'export-block-risu-save',
            destination,
            expectedRevision,
            omitAccount: options.omitAccount ?? false,
        },
    }) as { jobId: string; warningCodes?: string[] }
    let cancellationRequested = false
    let terminal: NativeFileJobStatus | undefined

    while (!terminal) {
        if (options.signal?.aborted && !cancellationRequested) {
            cancellationRequested = true
            await invokeNative(dependencies, 'native_file_job_cancel', { jobId: started.jobId })
        }
        const status = await invokeNative(dependencies, 'native_file_job_status', {
            jobId: started.jobId,
        }) as NativeFileJobStatus
        options.onStatus?.(status)
        if (status.state === 'succeeded' || status.state === 'failed' || status.state === 'cancelled') {
            terminal = status
            break
        }
        await dependencies.wait(options.pollIntervalMs ?? 100)
    }

    let outcomeFailed = false
    let committedResult: NativeFileJobResult | undefined
    try {
        if (terminal.state === 'succeeded') {
            if (!terminal.result) {
                throw new NativeFileJobError('missing-result', 'Native export returned no result')
            }
            committedResult = {
                ...terminal.result,
                warningCodes: [...new Set([
                    ...(started.warningCodes ?? []),
                    ...terminal.result.warningCodes,
                ])].slice(0, 16),
            }
            return committedResult
        }

        if (terminal.state === 'cancelled') throw abortError()
        throw new NativeFileJobError(
            terminal.error?.code ?? 'export-failed',
            terminal.error?.message ?? 'Native block RisuSave export failed',
        )
    }
    catch (error) {
        outcomeFailed = true
        throw error
    }
    finally {
        try {
            await invokeNative(dependencies, 'native_file_job_forget', { jobId: started.jobId })
        }
        catch (error) {
            if (committedResult) {
                committedResult.warningCodes = [
                    ...committedResult.warningCodes
                        .filter((code) => code !== 'cleanup-failed')
                        .slice(0, 15),
                    'cleanup-failed',
                ]
            }
            else if (!outcomeFailed) throw error
        }
    }
}
