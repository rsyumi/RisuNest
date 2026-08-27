import { invoke } from '@tauri-apps/api/core'

import { isTauri } from '../platform'
import type { PreparedNativeCharacterCardModule } from '../characterCards'
import {
    copyNativeExportToAndroidSaf,
    type AndroidSafDestinationRequest,
    type AndroidSafDestinationResult,
} from './androidSafBridge'

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
    handoffPath?: string
    recoveryPath?: string
}

export interface PreparedContentAssetDescriptor {
    referenceKey: string
    token: string
    logicalId: string
    objectHash: string
    byteSize: number
    mime: string
    name: string
    ext: string
}

export interface PreparedNativeContent {
    format: 'json-card' | 'charx-card' | 'appended-charx-jpeg'
    metadata: Record<string, unknown>
    assets: PreparedContentAssetDescriptor[]
    portraitLogicalId?: string
    module?: PreparedNativeCharacterCardModule
}

export interface NativeFileJobStatus {
    jobId: string
    kind:
        | 'restore-block-risu-save'
        | 'export-block-risu-save'
        | 'restore-lossless-backup'
        | 'export-lossless-backup'
        | 'prepare-content-import'
        | 'kei-backup-upload'
    expectedRevision?: number
    warningCodes?: string[]
    state: NativeFileJobState
    phase:
        | 'queued'
        | 'reading-source'
        | 'awaiting-content-mapping'
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
    preparedContent?: PreparedNativeContent
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

export interface PreparedNativeContentReceipt {
    readonly jobId: string
    readonly content: PreparedNativeContent
    readonly warningCodes: string[]
    confirmActivated(): Promise<void>
    cancel(): Promise<void>
}

export interface NativeFileRestoreJobOptions extends NativeFileJobOptions {
    afterRefresh?(): void | Promise<void>
    onBlockingChange?(blocking: boolean): void
}

export interface NativeFileExportJobOptions extends NativeFileJobOptions {
    omitAccount?: boolean
}

export type NativeLosslessBackupDestination =
    | { type: 'desktopPath'; path: string }
    | { type: 'androidSaf'; suggestedName: string }

export interface NativeFileJobDependencies {
    isTauri(): boolean
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    wait(milliseconds: number): Promise<void>
}

export interface NativeLosslessBackupDependencies extends NativeFileJobDependencies {
    copyToAndroidSaf(request: AndroidSafDestinationRequest): Promise<AndroidSafDestinationResult>
}

const productionDependencies: NativeFileJobDependencies = {
    isTauri: () => isTauri,
    invoke: (command, args) => args === undefined ? invoke(command) : invoke(command, args),
    wait: (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds)),
}

const productionLosslessDependencies: NativeLosslessBackupDependencies = {
    ...productionDependencies,
    copyToAndroidSaf: (request) => copyNativeExportToAndroidSaf(request),
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

function isTerminalJob(status: NativeFileJobStatus): boolean {
    return status.state === 'succeeded'
        || status.state === 'failed'
        || status.state === 'cancelled'
}

function preparedContentError(message: string): NativeFileJobError {
    return new NativeFileJobError('invalid-prepared-content', message)
}

function requiredDescriptorString(
    value: unknown,
    field: string,
): string {
    if (typeof value !== 'string' || value.length === 0) {
        throw preparedContentError(`Prepared content asset ${field} must be a nonempty string`)
    }
    return value
}

function validatePreparedContent(value: unknown): PreparedNativeContent {
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
        throw preparedContentError('Prepared content must be an object')
    }
    const content = value as Record<string, unknown>
    if (
        content.format !== 'json-card'
        && content.format !== 'charx-card'
        && content.format !== 'appended-charx-jpeg'
    ) {
        throw preparedContentError('Prepared content format is unsupported')
    }
    if (
        typeof content.metadata !== 'object'
        || content.metadata === null
        || Array.isArray(content.metadata)
    ) {
        throw preparedContentError('Prepared content metadata must be an object')
    }
    if (!Array.isArray(content.assets)) {
        throw preparedContentError('Prepared content assets must be an array')
    }
    const expectedContentFields = ['assets', 'format', 'metadata']
    if (content.portraitLogicalId !== undefined) expectedContentFields.push('portraitLogicalId')
    if (content.module !== undefined) expectedContentFields.push('module')
    if (Object.keys(content).sort().join('\0') !== expectedContentFields.sort().join('\0')) {
        throw preparedContentError('Prepared content fields are invalid')
    }
    const expectedFields = [
        'referenceKey',
        'token',
        'logicalId',
        'objectHash',
        'byteSize',
        'mime',
        'name',
        'ext',
    ].sort()
    const tokens = new Set<string>()
    const assets = content.assets.map((value, index): PreparedContentAssetDescriptor => {
        if (typeof value !== 'object' || value === null || Array.isArray(value)) {
            throw preparedContentError(`Prepared content asset ${index} must be an object`)
        }
        const asset = value as Record<string, unknown>
        if (Object.keys(asset).sort().join('\0') !== expectedFields.join('\0')) {
            throw preparedContentError(`Prepared content asset ${index} fields are invalid`)
        }
        const objectHash = requiredDescriptorString(asset.objectHash, 'objectHash')
        if (!/^[0-9a-f]{64}$/.test(objectHash)) {
            throw preparedContentError(`Prepared content asset ${index} objectHash is invalid`)
        }
        if (!Number.isSafeInteger(asset.byteSize) || (asset.byteSize as number) < 0) {
            throw preparedContentError(`Prepared content asset ${index} byteSize is invalid`)
        }
        const token = requiredDescriptorString(asset.token, 'token')
        if (tokens.has(token)) {
            throw preparedContentError(`Prepared content asset ${index} token is duplicated`)
        }
        tokens.add(token)
        const ext = requiredDescriptorString(asset.ext, 'ext')
        if (ext.length > 32 || !/^[A-Za-z0-9+_-]+$/.test(ext)) {
            throw preparedContentError(`Prepared content asset ${index} ext is invalid`)
        }
        const logicalId = requiredDescriptorString(asset.logicalId, 'logicalId')
        const logicalPrefix = `assets/${objectHash}.`
        if (!logicalId.startsWith(logicalPrefix)) {
            throw preparedContentError(`Prepared content asset ${index} logicalId does not match its object`)
        }
        const logicalSuffix = logicalId.slice(logicalPrefix.length)
        if (logicalSuffix.length > 32 || !/^[A-Za-z0-9+_-]+$/.test(logicalSuffix)) {
            throw preparedContentError(`Prepared content asset ${index} logicalId suffix is invalid`)
        }
        if (typeof asset.name !== 'string') {
            throw preparedContentError(`Prepared content asset ${index} name must be a string`)
        }
        return {
            referenceKey: requiredDescriptorString(asset.referenceKey, 'referenceKey'),
            token,
            logicalId,
            objectHash,
            byteSize: asset.byteSize as number,
            mime: requiredDescriptorString(asset.mime, 'mime'),
            name: asset.name,
            ext,
        }
    })
    let portraitLogicalId: string | undefined
    if (content.portraitLogicalId !== undefined) {
        portraitLogicalId = requiredDescriptorString(content.portraitLogicalId, 'portraitLogicalId')
        if (!assets.some((asset) => asset.logicalId === portraitLogicalId)) {
            throw preparedContentError('Prepared content portraitLogicalId must reference a prepared asset')
        }
    }
    let module: PreparedNativeCharacterCardModule | undefined
    if (content.module !== undefined) {
        if (typeof content.module !== 'object' || content.module === null || Array.isArray(content.module)) {
            throw preparedContentError('Prepared content module must be an object')
        }
        const rawModule = content.module as Record<string, unknown>
        const expectedModuleFields = ['lorebook', 'regex', 'trigger']
        if (!Object.keys(rawModule).every((field) => expectedModuleFields.includes(field))) {
            throw preparedContentError('Prepared content module fields are invalid')
        }
        for (const field of expectedModuleFields) {
            if (rawModule[field] !== undefined && !Array.isArray(rawModule[field])) {
                throw preparedContentError(`Prepared content module ${field} must be an array`)
            }
        }
        module = {
            ...(rawModule.trigger === undefined ? {} : { trigger: rawModule.trigger as PreparedNativeCharacterCardModule['trigger'] }),
            ...(rawModule.regex === undefined ? {} : { regex: rawModule.regex as PreparedNativeCharacterCardModule['regex'] }),
            ...(rawModule.lorebook === undefined ? {} : { lorebook: rawModule.lorebook as PreparedNativeCharacterCardModule['lorebook'] }),
        }
    }
    return {
        format: content.format,
        metadata: content.metadata as Record<string, unknown>,
        assets,
        ...(portraitLogicalId === undefined ? {} : { portraitLogicalId }),
        ...(module === undefined ? {} : { module }),
    }
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

async function runNativeReplacementRestore(
    kind: 'restore-block-risu-save' | 'restore-lossless-backup',
    mutationReason: string,
    runtime: NativeBlockRestoreRuntime,
    source: NativeFileJobSource,
    options: NativeFileRestoreJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    const operation = kind === 'restore-lossless-backup'
        ? 'Native lossless backup restore'
        : 'Native block RisuSave restore'
    if (!dependencies.isTauri()) {
        throw new Error(`${operation} requires Tauri`)
    }
    if (options.signal?.aborted) throw abortError()

    const mutationToken = await runtime.capturePersistentMutationToken(
        mutationReason,
    )
    if (options.signal?.aborted) throw abortError()
    const started = await invokeNative(dependencies, 'native_file_job_start', {
        request: {
            kind,
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
                throw new NativeFileJobError('missing-result', `${operation} returned no result`)
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
                terminal.error?.message ?? `${operation} failed`,
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

export function runNativeBlockRisuSaveRestore(
    runtime: NativeBlockRestoreRuntime,
    source: NativeFileJobSource,
    options: NativeFileRestoreJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    return runNativeReplacementRestore(
        'restore-block-risu-save',
        'native-block-risu-save-restore',
        runtime,
        source,
        options,
        dependencies,
    )
}

export function runNativeLosslessBackupRestore(
    runtime: NativeBlockRestoreRuntime,
    source: NativeFileJobSource,
    options: NativeFileRestoreJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    return runNativeReplacementRestore(
        'restore-lossless-backup',
        'native-lossless-backup-restore',
        runtime,
        source,
        options,
        dependencies,
    )
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

export async function runNativeLosslessBackupExport(
    runtime: {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    destination: NativeLosslessBackupDestination,
    options: NativeFileJobOptions = {},
    dependencies: NativeLosslessBackupDependencies = productionLosslessDependencies,
): Promise<NativeFileJobResult> {
    if (!dependencies.isTauri()) {
        throw new Error('Native lossless backup export requires Tauri')
    }
    if (options.signal?.aborted) throw abortError()

    await runtime.flushPendingData('native-lossless-backup-export')
    if (options.signal?.aborted) throw abortError()
    const expectedRevision = runtime.revision
    const request = destination.type === 'desktopPath'
        ? {
            kind: 'export-lossless-backup',
            destination: destination.path,
            expectedRevision,
        }
        : {
            kind: 'export-lossless-backup',
            expectedRevision,
        }
    const started = await invokeNative(dependencies, 'native_file_job_start', {
        request,
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
    let managedSource: string | undefined
    try {
        if (terminal.state === 'cancelled') throw abortError()
        if (terminal.state !== 'succeeded') {
            throw new NativeFileJobError(
                terminal.error?.code ?? 'export-failed',
                terminal.error?.message ?? 'Native lossless backup export failed',
            )
        }
        if (!terminal.result) {
            throw new NativeFileJobError('missing-result', 'Native lossless backup export returned no result')
        }
        committedResult = {
            ...terminal.result,
            warningCodes: [...new Set([
                ...(started.warningCodes ?? []),
                ...terminal.result.warningCodes,
            ])].slice(0, 16),
        }
        if (destination.type === 'androidSaf') {
            managedSource = committedResult.handoffPath
            if (!managedSource) {
                throw new NativeFileJobError(
                    'missing-handoff',
                    'Native lossless backup export returned no Android handoff path',
                )
            }
            const published = await dependencies.copyToAndroidSaf({
                sourcePath: managedSource,
                suggestedName: destination.suggestedName,
                signal: options.signal,
            })
            if (published.bytes !== committedResult.sourceBytes) {
                throw new NativeFileJobError(
                    'length-mismatch',
                    'Android SAF lossless backup length differs from its native source',
                )
            }
            const { handoffPath: _handoffPath, ...publishedResult } = committedResult
            committedResult = {
                ...publishedResult,
                warningCodes: [...new Set([
                    ...publishedResult.warningCodes,
                    ...published.warningCodes,
                ])].slice(0, 16),
            }
        }
        return committedResult
    }
    catch (error) {
        outcomeFailed = true
        throw error
    }
    finally {
        if (managedSource) {
            try {
                await invokeNative(dependencies, 'native_lossless_handoff_cleanup', {
                    path: managedSource,
                })
            }
            catch (error) {
                if (committedResult && !outcomeFailed) {
                    committedResult.warningCodes = [
                        ...committedResult.warningCodes
                            .filter((code) => code !== 'cleanup-failed')
                            .slice(0, 15),
                        'cleanup-failed',
                    ]
                }
            }
        }
        try {
            await invokeNative(dependencies, 'native_file_job_forget', { jobId: started.jobId })
        }
        catch (error) {
            if (committedResult && !outcomeFailed) {
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

export async function prepareNativeContentImport(
    source: NativeFileJobSource,
    displayName: string,
    options: NativeFileJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<PreparedNativeContentReceipt> {
    if (!dependencies.isTauri()) {
        throw new Error('Native content preparation requires Tauri')
    }
    if (options.signal?.aborted) throw abortError()

    const started = await invokeNative(dependencies, 'native_file_job_start', {
        request: {
            kind: 'prepare-content-import',
            source,
            displayName,
        },
    }) as { jobId: string; warningCodes?: string[] }
    let cancellationRequested = false

    const forget = async (): Promise<void> => {
        await invokeNative(dependencies, 'native_file_job_forget', { jobId: started.jobId })
    }
    const forgetBestEffort = async (): Promise<void> => {
        try {
            await forget()
        }
        catch {}
    }
    const cancelAndDrain = async (reportStatus = true): Promise<void> => {
        if (!cancellationRequested) {
            cancellationRequested = true
            await invokeNative(dependencies, 'native_file_job_cancel', { jobId: started.jobId })
        }
        while (true) {
            const status = await invokeNative(dependencies, 'native_file_job_status', {
                jobId: started.jobId,
            }) as NativeFileJobStatus
            if (reportStatus) options.onStatus?.(status)
            if (isTerminalJob(status)) break
            await dependencies.wait(options.pollIntervalMs ?? 100)
        }
        await forgetBestEffort()
    }

    let lastStatus: NativeFileJobStatus | undefined
    let cleanupAttempted = false
    try {
        while (true) {
            if (options.signal?.aborted) {
                await cancelAndDrain()
                cleanupAttempted = true
                throw abortError()
            }
            const status = await invokeNative(dependencies, 'native_file_job_status', {
                jobId: started.jobId,
            }) as NativeFileJobStatus
            lastStatus = status
            options.onStatus?.(status)
            if (options.signal?.aborted) {
                if (isTerminalJob(status)) await forgetBestEffort()
                else await cancelAndDrain()
                cleanupAttempted = true
                throw abortError()
            }
            if (!isTerminalJob(status)) {
                await dependencies.wait(options.pollIntervalMs ?? 100)
                continue
            }
            if (status.state === 'cancelled') {
                await forgetBestEffort()
                cleanupAttempted = true
                throw abortError()
            }
            if (status.state === 'failed') {
                const error = new NativeFileJobError(
                    status.error?.code ?? 'content-prepare-failed',
                    status.error?.message ?? 'Native content preparation failed',
                )
                await forgetBestEffort()
                cleanupAttempted = true
                throw error
            }

            const content = validatePreparedContent(status.preparedContent)
            let forgotten = false
            const acknowledge = async (): Promise<void> => {
                if (forgotten) return
                await forget()
                forgotten = true
            }
            return {
                jobId: started.jobId,
                content,
                warningCodes: [...new Set([
                    ...(started.warningCodes ?? []),
                    ...(status.warningCodes ?? []),
                ])].slice(0, 16),
                confirmActivated: acknowledge,
                cancel: acknowledge,
            }
        }
    }
    catch (error) {
        if (!cleanupAttempted) {
            if (lastStatus && isTerminalJob(lastStatus)) {
                await forgetBestEffort()
            }
            else {
                try {
                    await cancelAndDrain(false)
                }
                catch {}
            }
        }
        throw error
    }
}
