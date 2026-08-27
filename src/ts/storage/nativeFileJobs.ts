import { invoke } from '@tauri-apps/api/core'

import { isTauri } from '../platform'
import type { PreparedNativeCharacterCardModule } from '../characterCards'
import {
    finalizeContentCasJob,
    releaseCasJob,
    sealPreparedContentCasJob,
} from './nativeAssetRepository'
import type { PreparedImmutablePayload } from './payloadCas'
import {
    copyNativeExportToAndroidSaf,
    discardAndroidSafSource,
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
    publication?: NativeOfficialPublicationAttemptResult
}

export interface NativeOfficialAccountSnapshotRestoreRequest {
    baseUrl: string
    credential:
        | { kind: 'risu-auth'; token: string }
        | { kind: 'bearer'; token: string }
}

export type NativeOfficialAccountSnapshotRestoreResult =
    | { kind: 'missing' }
    | { kind: 'compatibility-fallback' }
    | {
        kind: 'activated'
        revision: number
        sourceSha256: string
        recoveryPath?: string
        warningCodes: string[]
}

interface PreparedContentAssetDescriptorBase {
    referenceKey?: string
    token?: string
    position?: number
    logicalId: string
    objectHash: string
    byteSize: number
    mime: string
    name: string
    ext: string
}

export interface PreparedCardContentAssetDescriptor extends PreparedContentAssetDescriptorBase {
    referenceKey: string
    token: string
}

export interface PreparedRisumContentAssetDescriptor extends PreparedContentAssetDescriptorBase {
    position: number
}

export type PreparedContentAssetDescriptor =
    | PreparedCardContentAssetDescriptor
    | PreparedRisumContentAssetDescriptor

export type PreparedRisumOwnerHead =
    | { present: false; manifestHash: null; entryCount: 0 }
    | { present: true; manifestHash: string; entryCount: number }

export interface PreparedNativeContent {
    casSessionId: string
    format: 'json-card' | 'png-card' | 'charx-card' | 'appended-charx-jpeg' | 'risu-module'
    metadata: Record<string, unknown>
    assets: PreparedContentAssetDescriptor[]
    portraitLogicalId?: string
    module?: PreparedNativeCharacterCardModule
    ownerHead?: PreparedRisumOwnerHead
}

export interface PreparedNativeRisumContent extends PreparedNativeContent {
    format: 'risu-module'
    assets: PreparedRisumContentAssetDescriptor[]
    ownerHead: PreparedRisumOwnerHead
}

interface NativeOfficialPublicationCommonResult {
    accountId: string
    session: string | null
    saveDate: string
    status: number
}

export type NativeOfficialPublicationAttemptResult =
    | (NativeOfficialPublicationCommonResult & {
          kind: 'written'
          replacementKey: string
          warning: string | null
          reloadSession: boolean
      })
    | (NativeOfficialPublicationCommonResult & {
          kind: 'not-modified'
          replacementKey: string
      })
    | (NativeOfficialPublicationCommonResult & { kind: 'auth-warning' })
    | (NativeOfficialPublicationCommonResult & { kind: 'reauthentication-needed' })

export interface NativeOfficialPublicationRequest {
    expectedRevision: number
    lease: string
    accountId: string
    baseUrl: string
    replacements: Readonly<Record<string, string>>
    session: string | null
    saveDate: string
    credential: {
        kind: 'risu-auth'
        token: string
    }
}

export interface NativeOfficialPublicationReceipt {
    jobId: string
    result: NativeFileJobResult & {
        publication: NativeOfficialPublicationAttemptResult
    }
    acknowledge(): Promise<void>
}

export type NativeOfficialPublicationRunResult =
    | {
          kind: 'waiting-for-reauthentication'
          jobId: string
          accountId: string
          session: string | null
      }
    | {
          kind: 'completed'
          receipt: NativeOfficialPublicationReceipt
      }

export interface NativeFileJobStatus {
    jobId: string
    kind:
        | 'restore-block-risu-save'
        | 'export-block-risu-save'
        | 'restore-lossless-backup'
        | 'export-lossless-backup'
        | 'export-character-charx'
        | 'restore-legacy-local-backup'
        | 'export-legacy-local-backup'
        | 'prepare-content-import'
        | 'import-jpeg-asset'
        | 'kei-backup-upload'
        | 'restore-official-account-snapshot'
        | 'official-publication-upload'
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
        | 'uploading-database'
        | 'awaiting-publication-retry'
        | 'finalizing-publication'
        | 'publishing-destination'
        | 'finalizing-export'
        | 'complete'
    progress: {
        completedBytes: number
        totalBytes?: number
        completedItems: number
        totalItems?: number
    }
    publicationAttempt?: NativeOfficialPublicationAttemptResult
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

export interface PreparedNativeContentActivationLifecycle {
    prepareOwnerManifestAndSeal(bytes: Uint8Array): Promise<PreparedImmutablePayload>
    sealPreparedContent?(): Promise<void>
}

export interface PreparedNativeContentReceipt extends PreparedNativeContentActivationLifecycle {
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

export interface NativeCharacterCharxExportInput {
    characterId: string
    destination: NativeCharacterCharxExportDestination
    expectedRevision: number
    card: Record<string, unknown>
    module: Record<string, unknown>
}

export type NativeCharacterCharxExportDestination =
    | { type: 'desktopPath'; path: string }
    | { type: 'androidSaf'; suggestedName: string }

export type NativeLosslessBackupDestination =
    | { type: 'desktopPath'; path: string }
    | { type: 'androidSaf'; suggestedName: string }

export type NativeLegacyLocalBackupDestination = NativeLosslessBackupDestination

export interface NativeFileJobDependencies {
    isTauri(): boolean
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    wait(milliseconds: number): Promise<void>
    discardAndroidSource?(token: string): boolean
}

export interface NativeLosslessBackupDependencies extends NativeFileJobDependencies {
    copyToAndroidSaf(request: AndroidSafDestinationRequest): Promise<AndroidSafDestinationResult>
}

const productionDependencies: NativeFileJobDependencies = {
    isTauri: () => isTauri,
    invoke: (command, args) => args === undefined ? invoke(command) : invoke(command, args),
    wait: (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds)),
    discardAndroidSource: (token) => discardAndroidSafSource(token),
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

function validatePreparedContent(value: unknown, expectedCasSessionId: string): PreparedNativeContent {
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
        throw preparedContentError('Prepared content must be an object')
    }
    const content = value as Record<string, unknown>
    if (
        content.format !== 'json-card'
        && content.format !== 'png-card'
        && content.format !== 'charx-card'
        && content.format !== 'appended-charx-jpeg'
        && content.format !== 'risu-module'
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
    const casSessionId = requiredDescriptorString(content.casSessionId, 'casSessionId')
    if (casSessionId !== expectedCasSessionId) {
        throw preparedContentError('Prepared content casSessionId must match its native job')
    }
    const expectedContentFields = ['assets', 'casSessionId', 'format', 'metadata']
    if (content.portraitLogicalId !== undefined) expectedContentFields.push('portraitLogicalId')
    if (content.module !== undefined) expectedContentFields.push('module')
    if (content.ownerHead !== undefined) expectedContentFields.push('ownerHead')
    if (Object.keys(content).sort().join('\0') !== expectedContentFields.sort().join('\0')) {
        throw preparedContentError('Prepared content fields are invalid')
    }
    const expectedCardFields = [
        'referenceKey',
        'token',
        'logicalId',
        'objectHash',
        'byteSize',
        'mime',
        'name',
        'ext',
    ].sort()
    const expectedRisumFields = [
        'position',
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
        const risum = content.format === 'risu-module'
        const expectedFields = risum ? expectedRisumFields : expectedCardFields
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
        let token: string | undefined
        if (!risum) {
            token = requiredDescriptorString(asset.token, 'token')
            if (tokens.has(token)) {
                throw preparedContentError(`Prepared content asset ${index} token is duplicated`)
            }
            tokens.add(token)
        }
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
        if (typeof asset.mime !== 'string') {
            throw preparedContentError(`Prepared content asset ${index} mime must be a string`)
        }
        const mime = asset.mime
        if (mime.length === 0 && content.format !== 'png-card' && !risum) {
            throw preparedContentError(`Prepared content asset ${index} mime must be a nonempty string`)
        }
        if (risum) {
            if (!Number.isSafeInteger(asset.position) || asset.position !== index) {
                throw preparedContentError(`Prepared RISUM asset ${index} position is invalid`)
            }
            return {
                position: index,
                logicalId,
                objectHash,
                byteSize: asset.byteSize as number,
                mime,
                name: asset.name,
                ext,
            }
        }
        return {
            referenceKey: requiredDescriptorString(asset.referenceKey, 'referenceKey'),
            token: token!,
            logicalId,
            objectHash,
            byteSize: asset.byteSize as number,
            mime,
            name: asset.name,
            ext,
        }
    })
    if (content.format === 'risu-module') {
        if (content.portraitLogicalId !== undefined || content.module !== undefined) {
            throw preparedContentError('Prepared RISUM content cannot contain card fields')
        }
        if (typeof content.ownerHead !== 'object' || content.ownerHead === null || Array.isArray(content.ownerHead)) {
            throw preparedContentError('Prepared RISUM ownerHead must be an object')
        }
        const ownerHead = content.ownerHead as Record<string, unknown>
        if (Object.keys(ownerHead).sort().join('\0') !== ['entryCount', 'manifestHash', 'present'].join('\0')) {
            throw preparedContentError('Prepared RISUM ownerHead fields are invalid')
        }
        if (typeof ownerHead.present !== 'boolean') {
            throw preparedContentError('Prepared RISUM ownerHead present is invalid')
        }
        if (!Number.isSafeInteger(ownerHead.entryCount) || (ownerHead.entryCount as number) < 0) {
            throw preparedContentError('Prepared RISUM ownerHead entryCount is invalid')
        }
        if (ownerHead.present) {
            if (typeof ownerHead.manifestHash !== 'string' || !/^[0-9a-f]{64}$/.test(ownerHead.manifestHash)) {
                throw preparedContentError('Prepared RISUM ownerHead manifestHash is invalid')
            }
            if (ownerHead.entryCount !== assets.length) {
                throw preparedContentError('Prepared RISUM ownerHead entryCount does not match assets')
            }
        }
        else if (ownerHead.manifestHash !== null || ownerHead.entryCount !== 0 || assets.length !== 0) {
            throw preparedContentError('Absent RISUM ownerHead must have no assets')
        }
        return {
            casSessionId,
            format: 'risu-module',
            metadata: content.metadata as Record<string, unknown>,
            assets: assets as PreparedRisumContentAssetDescriptor[],
            ownerHead: ownerHead as PreparedRisumOwnerHead,
        }
    }
    const cardAssets = assets as PreparedCardContentAssetDescriptor[]
    let portraitLogicalId: string | undefined
    if (content.portraitLogicalId !== undefined) {
        portraitLogicalId = requiredDescriptorString(content.portraitLogicalId, 'portraitLogicalId')
        if (!cardAssets.some((asset) => asset.logicalId === portraitLogicalId)) {
            throw preparedContentError('Prepared content portraitLogicalId must reference a prepared asset')
        }
    }
    if (content.format === 'png-card') {
        const metadata = content.metadata as Record<string, unknown>
        if (!Object.keys(metadata).every((field) => field === 'chara' || field === 'ccv3')) {
            throw preparedContentError('Prepared PNG metadata fields are invalid')
        }
        const encodedMetadata = [metadata.chara, metadata.ccv3]
        if (!encodedMetadata.some((value) => typeof value === 'string' && value.length > 0)) {
            throw preparedContentError('Prepared PNG card metadata is missing')
        }
        for (const value of encodedMetadata) {
            if (value !== undefined && (typeof value !== 'string' || value.length === 0)) {
                throw preparedContentError('Prepared PNG card metadata must be a nonempty string')
            }
            if (typeof value === 'string' && value.length > 5 * 1024 * 1024) {
                throw preparedContentError('Prepared PNG card metadata exceeds the 5 MiB limit')
            }
        }
        if (content.module !== undefined) {
            throw preparedContentError('Prepared PNG content cannot contain a module')
        }
        if (!portraitLogicalId) {
            throw preparedContentError('Prepared PNG portraitLogicalId is required')
        }
        const portrait = cardAssets[0]
        if (!portrait || portrait.logicalId !== portraitLogicalId) {
            throw preparedContentError('Prepared PNG portrait must be the first asset')
        }
        if (
            !/^native-png-portrait(?:-[1-9][0-9]*)?$/.test(portrait.token)
            || portrait.referenceKey !== portrait.token
            || portrait.mime !== 'image/png'
            || portrait.ext !== 'png'
            || portrait.logicalId !== `assets/${portrait.objectHash}.png`
            || portrait.name !== `${portrait.objectHash}.png`
        ) {
            throw preparedContentError('Prepared PNG portrait descriptor is invalid')
        }
        for (const [index, asset] of cardAssets.slice(1).entries()) {
            if (asset.token !== asset.referenceKey) {
                throw preparedContentError(`Prepared PNG embedded asset ${index} token must equal referenceKey`)
            }
            if (asset.mime !== '') {
                throw preparedContentError(`Prepared PNG embedded asset ${index} mime must be empty`)
            }
            if (
                asset.ext !== 'png'
                || asset.logicalId !== `assets/${asset.objectHash}.png`
                || asset.name !== `${asset.objectHash}.png`
            ) {
                throw preparedContentError(`Prepared PNG embedded asset ${index} descriptor is invalid`)
            }
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
        casSessionId,
        format: content.format,
        metadata: content.metadata as Record<string, unknown>,
        assets: cardAssets,
        ...(portraitLogicalId === undefined ? {} : { portraitLogicalId }),
        ...(module === undefined ? {} : { module }),
    }
}

function drainedNativeOfficialPublicationError(error: Error): Error {
    return Object.assign(error, {
        nativeOfficialPublicationCancellationDrained: true,
    })
}

function cancelledNativeOfficialPublicationAbortError(): Error {
    return drainedNativeOfficialPublicationError(abortError())
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

function abortBeforeNativeRestoreStart(
    source: NativeFileJobSource | NativeOfficialAccountSnapshotRestoreRequest,
    dependencies: NativeFileJobDependencies,
): never {
    if (!('type' in source)) throw abortError()
    let discarded = source.type !== 'androidSpool'
    if (source.type === 'androidSpool') {
        try {
            discarded = dependencies.discardAndroidSource?.(source.token) === true
        }
        catch {}
    }
    if (!discarded) {
        throw new NativeFileJobError(
            'cleanup-failed',
            'Cancelled Android source could not be discarded before native restore start',
        )
    }
    throw abortError()
}

async function runNativeReplacementRestore(
    kind:
        | 'restore-block-risu-save'
        | 'restore-lossless-backup'
        | 'restore-legacy-local-backup'
        | 'restore-official-account-snapshot',
    mutationReason: string,
    runtime: NativeBlockRestoreRuntime,
    source: NativeFileJobSource | NativeOfficialAccountSnapshotRestoreRequest,
    options: NativeFileRestoreJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    const operation = kind === 'restore-lossless-backup'
        ? 'Native lossless backup restore'
        : kind === 'restore-official-account-snapshot'
            ? 'Native official account snapshot restore'
        : kind === 'restore-legacy-local-backup'
            ? 'Native legacy local backup restore'
        : 'Native block RisuSave restore'
    if (!dependencies.isTauri()) {
        throw new Error(`${operation} requires Tauri`)
    }
    if (options.signal?.aborted) {
        abortBeforeNativeRestoreStart(source, dependencies)
    }

    const mutationToken = await runtime.capturePersistentMutationToken(
        mutationReason,
    )
    if (options.signal?.aborted) {
        abortBeforeNativeRestoreStart(source, dependencies)
    }
    const request = kind === 'restore-official-account-snapshot'
        ? {
            kind,
            ...(source as NativeOfficialAccountSnapshotRestoreRequest),
            expectedRevision: mutationToken.revision,
        }
        : {
            kind,
            source: source as NativeFileJobSource,
            expectedRevision: mutationToken.revision,
        }
    const started = await invokeNative(dependencies, 'native_file_job_start', { request }) as {
        jobId: string
        warningCodes?: string[]
    }
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

export async function runNativeOfficialAccountSnapshotRestore(
    runtime: NativeBlockRestoreRuntime,
    request: NativeOfficialAccountSnapshotRestoreRequest,
    options: NativeFileRestoreJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeOfficialAccountSnapshotRestoreResult> {
    let result: NativeFileJobResult
    try {
        result = await runNativeReplacementRestore(
            'restore-official-account-snapshot',
            'native-official-account-snapshot-restore',
            runtime,
            request,
            options,
            dependencies,
        )
    }
    catch (error) {
        if (error instanceof NativeFileJobError && error.code === 'remote-missing') {
            return { kind: 'missing' }
        }
        if (error instanceof NativeFileJobError && error.code === 'compatibility-required') {
            return { kind: 'compatibility-fallback' }
        }
        throw error
    }
    return {
        kind: 'activated',
        revision: result.revision,
        sourceSha256: result.sourceSha256,
        recoveryPath: result.recoveryPath,
        warningCodes: result.warningCodes,
    }
}

export function runNativeLegacyLocalBackupRestore(
    runtime: NativeBlockRestoreRuntime,
    source: NativeFileJobSource,
    options: NativeFileRestoreJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    return runNativeReplacementRestore(
        'restore-legacy-local-backup',
        'native-legacy-local-backup-restore',
        runtime,
        source,
        options,
        dependencies,
    )
}

async function runNativePathExport(
    kind: 'export-block-risu-save' | 'export-legacy-local-backup',
    flushReason: string,
    operation: string,
    runtime: {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    destination: string,
    options: NativeFileExportJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    if (!dependencies.isTauri()) {
        throw new Error(`${operation} requires Tauri`)
    }
    if (options.signal?.aborted) throw abortError()

    await runtime.flushPendingData(flushReason)
    if (options.signal?.aborted) throw abortError()
    const expectedRevision = runtime.revision
    const started = await invokeNative(dependencies, 'native_file_job_start', {
        request: {
            kind,
            destination,
            expectedRevision,
            ...(kind === 'export-block-risu-save'
                ? { omitAccount: options.omitAccount ?? false }
                : {}),
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


export function runNativeBlockRisuSaveExport(
    runtime: {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    destination: string,
    options: NativeFileExportJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    return runNativePathExport(
        'export-block-risu-save',
        'native-block-risu-save-export',
        'Native block RisuSave export',
        runtime,
        destination,
        options,
        dependencies,
    )
}

export async function runNativeCharacterCharxExport(
    input: NativeCharacterCharxExportInput,
    options: NativeFileJobOptions = {},
    dependencies: NativeLosslessBackupDependencies = productionLosslessDependencies,
): Promise<NativeFileJobResult> {
    if (!dependencies.isTauri()) {
        throw new Error('Native character CharX export requires Tauri')
    }
    if (options.signal?.aborted) throw abortError()
    const started = await invokeNative(dependencies, 'native_file_job_start', {
        request: {
            kind: 'export-character-charx',
            ...(input.destination.type === 'desktopPath'
                ? { destination: input.destination.path }
                : {}),
            expectedRevision: input.expectedRevision,
            characterId: input.characterId,
            card: input.card,
            module: input.module,
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
        if (isTerminalJob(status)) terminal = status
        else await dependencies.wait(options.pollIntervalMs ?? 100)
    }

    let outcomeFailed = false
    let result: NativeFileJobResult | undefined
    let managedSource: string | undefined
    let handoffCleanupFailed = false
    try {
        if (terminal.state === 'cancelled') throw abortError()
        if (terminal.state !== 'succeeded') {
            throw new NativeFileJobError(
                terminal.error?.code ?? 'export-failed',
                terminal.error?.message ?? 'Native character CharX export failed',
            )
        }
        if (!terminal.result) {
            throw new NativeFileJobError(
                'missing-result',
                'Native character CharX export returned no result',
            )
        }
        result = {
            ...terminal.result,
            warningCodes: [...new Set([
                ...(started.warningCodes ?? []),
                ...terminal.result.warningCodes,
            ])].slice(0, 16),
        }
        if (input.destination.type === 'androidSaf') {
            managedSource = result.handoffPath
            if (!managedSource) {
                throw new NativeFileJobError(
                    'missing-handoff',
                    'Native character CharX export returned no Android handoff path',
                )
            }
            const published = await dependencies.copyToAndroidSaf({
                sourcePath: managedSource,
                suggestedName: input.destination.suggestedName,
                signal: options.signal,
            })
            if (published.bytes !== result.sourceBytes) {
                throw new NativeFileJobError(
                    'length-mismatch',
                    'Android SAF character CharX length differs from its native source',
                )
            }
            const { handoffPath: _handoffPath, ...publishedResult } = result
            result = {
                ...publishedResult,
                warningCodes: [...new Set([
                    ...publishedResult.warningCodes,
                    ...published.warningCodes,
                ])].slice(0, 16),
            }
        }
        return result
    }
    catch (error) {
        outcomeFailed = true
        throw error
    }
    finally {
        if (managedSource) {
            try {
                await invokeNative(dependencies, 'native_character_charx_handoff_cleanup', {
                    path: managedSource,
                })
            }
            catch {
                handoffCleanupFailed = true
                if (result && !outcomeFailed) {
                    result.warningCodes = [
                        ...result.warningCodes
                            .filter((code) => code !== 'cleanup-failed')
                            .slice(0, 15),
                        'cleanup-failed',
                    ]
                }
            }
        }
        if (!handoffCleanupFailed) {
            try {
                await invokeNative(dependencies, 'native_file_job_forget', { jobId: started.jobId })
            }
            catch (error) {
                if (result) {
                    result.warningCodes = [
                        ...result.warningCodes.filter((code) => code !== 'cleanup-failed').slice(0, 15),
                        'cleanup-failed',
                    ]
                }
                else if (!outcomeFailed) throw error
            }
        }
    }
}

export function runNativeLegacyLocalBackupExport(
    runtime: {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    destination: NativeLegacyLocalBackupDestination,
    options: NativeFileJobOptions = {},
    dependencies: NativeLosslessBackupDependencies = productionLosslessDependencies,
): Promise<NativeFileJobResult> {
    return runNativePortableBackupExport(
        'export-legacy-local-backup',
        'native-legacy-local-backup-export',
        'Native legacy local backup export',
        'native_legacy_backup_handoff_cleanup',
        runtime,
        destination,
        options,
        dependencies,
    )
}

async function runNativePortableBackupExport(
    kind: 'export-lossless-backup' | 'export-legacy-local-backup',
    flushReason: string,
    operation: string,
    handoffCleanupCommand: string,
    runtime: {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    destination: NativeLosslessBackupDestination,
    options: NativeFileJobOptions = {},
    dependencies: NativeLosslessBackupDependencies = productionLosslessDependencies,
): Promise<NativeFileJobResult> {
    if (!dependencies.isTauri()) {
        throw new Error(`${operation} requires Tauri`)
    }
    if (options.signal?.aborted) throw abortError()

    await runtime.flushPendingData(flushReason)
    if (options.signal?.aborted) throw abortError()
    const expectedRevision = runtime.revision
    const request = destination.type === 'desktopPath'
        ? {
            kind,
            destination: destination.path,
            expectedRevision,
        }
        : {
            kind,
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
                terminal.error?.message ?? `${operation} failed`,
            )
        }
        if (!terminal.result) {
            throw new NativeFileJobError('missing-result', `${operation} returned no result`)
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
                    `${operation} returned no Android handoff path`,
                )
            }
            const published = await dependencies.copyToAndroidSaf({
                sourcePath: managedSource,
                suggestedName: destination.suggestedName,
                signal: options.signal,
                onProgress: (progress) => options.onStatus?.({
                    ...terminal,
                    state: 'running',
                    phase: 'publishing-destination',
                    progress: {
                        completedBytes: progress.copiedBytes,
                        ...(progress.totalBytes === null
                            ? { totalBytes: committedResult?.sourceBytes }
                            : { totalBytes: progress.totalBytes }),
                        completedItems: 0,
                        totalItems: 1,
                    },
                }),
            })
            if (published.bytes !== committedResult.sourceBytes) {
                throw new NativeFileJobError(
                    'length-mismatch',
                    `Android SAF ${operation} length differs from its native source`,
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
                await invokeNative(dependencies, handoffCleanupCommand, {
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

export function runNativeLosslessBackupExport(
    runtime: {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    destination: NativeLosslessBackupDestination,
    options: NativeFileJobOptions = {},
    dependencies: NativeLosslessBackupDependencies = productionLosslessDependencies,
): Promise<NativeFileJobResult> {
    return runNativePortableBackupExport(
        'export-lossless-backup',
        'native-lossless-backup-export',
        'Native lossless backup export',
        'native_lossless_handoff_cleanup',
        runtime,
        destination,
        options,
        dependencies,
    )
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
    const releaseAndForget = async (outcome: 'committed' | 'aborted'): Promise<void> => {
        let releaseFailed = false
        let releaseError: unknown
        try {
            await releaseCasJob(started.jobId, outcome, dependencies.invoke)
        }
        catch (error) {
            releaseFailed = true
            releaseError = error
        }
        let forgetFailed = false
        let forgetError: unknown
        try {
            await forget()
        }
        catch (error) {
            forgetFailed = true
            forgetError = error
        }
        if (releaseFailed) throw releaseError
        if (forgetFailed) throw forgetError
    }
    const abortAndForgetBestEffort = async (): Promise<void> => {
        try {
            await releaseAndForget('aborted')
        }
        catch {}
    }
    const cancelAndDrain = async (reportStatus = true): Promise<void> => {
        if (!cancellationRequested) {
            cancellationRequested = true
            await invokeNative(dependencies, 'native_file_job_cancel', { jobId: started.jobId })
        }
        let terminal: NativeFileJobStatus
        while (true) {
            const status = await invokeNative(dependencies, 'native_file_job_status', {
                jobId: started.jobId,
            }) as NativeFileJobStatus
            if (reportStatus) options.onStatus?.(status)
            if (isTerminalJob(status)) {
                terminal = status
                break
            }
            await dependencies.wait(options.pollIntervalMs ?? 100)
        }
        if (terminal.state === 'succeeded') await abortAndForgetBestEffort()
        else await forgetBestEffort()
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
                if (status.state === 'succeeded') await abortAndForgetBestEffort()
                else if (isTerminalJob(status)) await forgetBestEffort()
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

            const content = validatePreparedContent(status.preparedContent, started.jobId)
            let lifecycleState: 'unfinalized' | 'finalizing' | 'finalized' | 'settling' | 'settled' = 'unfinalized'
            let finalizerOperation: Promise<PreparedImmutablePayload> | undefined
            let cancellationDuringFinalize = false
            let settlement: Promise<void> | undefined
            const settle = (operation: () => Promise<void>): Promise<void> => {
                if (settlement) return settlement
                lifecycleState = 'settling'
                settlement = operation().finally(() => {
                    lifecycleState = 'settled'
                })
                return settlement
            }
            const prepareOwnerManifestAndSeal = async (bytes: Uint8Array): Promise<PreparedImmutablePayload> => {
                if (lifecycleState !== 'unfinalized') {
                    throw new Error('Native content can only be finalized once')
                }
                lifecycleState = 'finalizing'
                finalizerOperation = finalizeContentCasJob(
                    started.jobId,
                    bytes,
                    dependencies.invoke,
                )
                let prepared: PreparedImmutablePayload
                try {
                    prepared = await finalizerOperation
                }
                catch (error) {
                    if (!cancellationDuringFinalize) {
                        settle(() => releaseAndForget('aborted'))
                    }
                    try {
                        await settlement
                    }
                    catch {}
                    throw error
                }
                if (cancellationDuringFinalize) {
                    try {
                        await settlement
                    }
                    catch {}
                    throw abortError()
                }
                lifecycleState = 'finalized'
                return prepared
            }
            const sealPreparedContent = async (): Promise<void> => {
                if (lifecycleState !== 'unfinalized') {
                    throw new Error('Native content can only be finalized once')
                }
                lifecycleState = 'finalizing'
                finalizerOperation = sealPreparedContentCasJob(
                    started.jobId,
                    dependencies.invoke,
                ).then(() => ({
                    contentHash: '0'.repeat(64),
                    byteSize: 0,
                    physicalKey: '',
                    deduplicated: true,
                }))
                try {
                    await finalizerOperation
                }
                catch (error) {
                    if (!cancellationDuringFinalize) settle(() => releaseAndForget('aborted'))
                    try { await settlement }
                    catch {}
                    throw error
                }
                if (cancellationDuringFinalize) {
                    try { await settlement }
                    catch {}
                    throw abortError()
                }
                lifecycleState = 'finalized'
            }
            const confirmActivated = async (): Promise<void> => {
                if (lifecycleState === 'settled') return
                if (lifecycleState !== 'finalized') {
                    throw new Error('Native content activation cannot be confirmed before finalizing')
                }
                await settle(() => releaseAndForget('committed'))
            }
            const cancel = async (): Promise<void> => {
                if (lifecycleState === 'settled') return
                if (settlement) return settlement
                if (lifecycleState === 'finalizing') {
                    cancellationDuringFinalize = true
                    await settle(async () => {
                        try {
                            await finalizerOperation
                        }
                        catch {}
                        await releaseAndForget('aborted')
                    })
                    return
                }
                if (lifecycleState === 'unfinalized') {
                    await settle(() => releaseAndForget('aborted'))
                    return
                }
                await settle(forget)
            }
            return {
                jobId: started.jobId,
                content,
                warningCodes: [...new Set([
                    ...(started.warningCodes ?? []),
                    ...(status.warningCodes ?? []),
                ])].slice(0, 16),
                prepareOwnerManifestAndSeal,
                sealPreparedContent,
                confirmActivated,
                cancel,
            }
        }
    }
    catch (error) {
        if (!cleanupAttempted) {
            if (lastStatus?.state === 'succeeded') {
                await abortAndForgetBestEffort()
            }
            else if (lastStatus && isTerminalJob(lastStatus)) {
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

function assertOfficialPublicationResult(
    value: unknown,
): NativeOfficialPublicationAttemptResult {
    if (!value || typeof value !== 'object') {
        throw new NativeFileJobError(
            'invalid-result',
            'Native official publication returned invalid publication metadata',
        )
    }
    const result = value as Record<string, unknown>
    if (
        !isBoundedString(result.accountId, 512, false)
        || (result.session !== null && !isBoundedString(result.session, 4_096, true))
        || !isBoundedString(result.saveDate, 128, false)
        || !Number.isInteger(result.status)
        || (result.status as number) < 100
        || (result.status as number) > 599
    ) {
        throw new NativeFileJobError(
            'invalid-result',
            'Native official publication returned invalid publication metadata',
        )
    }
    switch (result.kind) {
        case 'written':
            if (
                isBoundedString(result.replacementKey, 4_096, false)
                && (result.warning === null || isBoundedString(result.warning, 4_096, true))
                && typeof result.reloadSession === 'boolean'
            ) {
                return result as unknown as NativeOfficialPublicationAttemptResult
            }
            break
        case 'not-modified':
            if (isBoundedString(result.replacementKey, 4_096, false)) {
                return result as unknown as NativeOfficialPublicationAttemptResult
            }
            break
        case 'auth-warning':
        case 'reauthentication-needed':
            return result as unknown as NativeOfficialPublicationAttemptResult
    }
    throw new NativeFileJobError(
        'invalid-result',
        'Native official publication returned invalid publication metadata',
    )
}

function isBoundedString(value: unknown, maximumLength: number, allowEmpty: boolean): value is string {
    return typeof value === 'string'
        && value.length <= maximumLength
        && (allowEmpty || value.length > 0)
}

function areBoundedWarningCodes(value: unknown): value is string[] {
    return Array.isArray(value)
        && value.length <= 64
        && value.every((code) => isBoundedString(code, 128, false))
}

function createOfficialPublicationReceipt(
    terminal: NativeFileJobStatus,
    dependencies: NativeFileJobDependencies,
    expected: {
        jobId: string
        revision?: number
        accountId?: string
        saveDate?: string
    },
    startWarningCodes: readonly string[] = [],
): NativeOfficialPublicationReceipt {
    if (terminal.state !== 'succeeded' || !terminal.result?.publication) {
        throw new NativeFileJobError(
            'missing-result',
            'Native official publication returned no result',
        )
    }
    const publication = assertOfficialPublicationResult(terminal.result.publication)
    if (
        terminal.jobId !== expected.jobId
        || terminal.kind !== 'official-publication-upload'
        || (expected.revision !== undefined && terminal.result.revision !== expected.revision)
        || (expected.accountId !== undefined && publication.accountId !== expected.accountId)
        || (expected.saveDate !== undefined && publication.saveDate !== expected.saveDate)
        || !Number.isSafeInteger(terminal.result.revision)
        || terminal.result.revision < 0
        || !Number.isSafeInteger(terminal.result.sourceBytes)
        || terminal.result.sourceBytes < 0
        || typeof terminal.result.sourceSha256 !== 'string'
        || !/^[0-9a-f]{64}$/.test(terminal.result.sourceSha256)
        || !Number.isSafeInteger(terminal.result.characterCount)
        || terminal.result.characterCount < 0
        || !Number.isSafeInteger(terminal.result.presetCount)
        || terminal.result.presetCount < 0
        || !areBoundedWarningCodes(startWarningCodes)
        || !areBoundedWarningCodes(terminal.warningCodes ?? [])
        || !areBoundedWarningCodes(terminal.result.warningCodes)
    ) {
        throw new NativeFileJobError(
            'invalid-result',
            'Native official publication returned mismatched association metadata',
        )
    }

    const result = {
        ...terminal.result,
        publication,
        warningCodes: [...new Set([
            ...startWarningCodes,
            ...(terminal.warningCodes ?? []),
            ...terminal.result.warningCodes,
        ])].slice(0, 16),
    } as NativeOfficialPublicationReceipt['result']
    let acknowledged = false
    let acknowledgement: Promise<void> | undefined

    return {
        jobId: terminal.jobId,
        result,
        acknowledge: async () => {
            if (acknowledged) return
            acknowledgement ??= invokeNative(dependencies, 'native_file_job_forget', {
                jobId: terminal.jobId,
            }).then(() => {
                acknowledged = true
            }).finally(() => {
                acknowledgement = undefined
            })
            await acknowledgement
        },
    }
}

export interface NativeOfficialPublicationRetryRequest {
    accountId: string
    session: string | null
    saveDate: string
    credential: {
        kind: 'risu-auth'
        token: string
    }
}

export async function runNativeOfficialPublicationAttempt(
    request: NativeOfficialPublicationRequest,
    options: NativeFileJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeOfficialPublicationRunResult | null> {
    if (!dependencies.isTauri()) {
        throw new Error('Native official publication requires Tauri')
    }
    if (options.signal?.aborted) throw abortError()

    let started: { jobId: string; warningCodes?: string[] }
    try {
        const value = await invokeNative(dependencies, 'native_file_job_start', {
            request: {
                kind: 'official-publication-upload',
                ...request,
            },
        })
        if (
            !value
            || typeof value !== 'object'
            || !('jobId' in value)
            || typeof value.jobId !== 'string'
            || value.jobId.length === 0
            || ('warningCodes' in value && (
                !Array.isArray(value.warningCodes)
                || value.warningCodes.some((code) => typeof code !== 'string')
            ))
        ) {
            throw new NativeFileJobError(
                'invalid-result',
                'Native official publication start returned no job ID',
            )
        }
        started = value as unknown as typeof started
    }
    catch (error) {
        if (error instanceof NativeFileJobError && error.code === 'capability-unavailable') {
            return null
        }
        throw error
    }
    return await pollNativeOfficialPublication(started.jobId, {
        jobId: started.jobId,
        revision: request.expectedRevision,
        accountId: request.accountId,
        saveDate: request.saveDate,
    }, options, dependencies, {
        startWarningCodes: started.warningCodes,
        terminalFailure: 'throw',
    })
}

export async function continueNativeOfficialPublication(
    jobId: string,
    request: NativeOfficialPublicationRetryRequest,
    expected: {
        revision: number
        accountId: string
    },
    options: NativeFileJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeOfficialPublicationRunResult> {
    if (!dependencies.isTauri()) {
        throw new Error('Native official publication requires Tauri')
    }
    if (options.signal?.aborted) {
        await cancelNativeOfficialPublication(jobId, {}, dependencies)
        throw cancelledNativeOfficialPublicationAbortError()
    }

    await invokeNative(dependencies, 'native_file_job_official_publication_retry', {
        request: {
            jobId,
            accountId: request.accountId,
            session: request.session,
            saveDate: request.saveDate,
            credential: request.credential,
        },
    })
    const outcome = await pollNativeOfficialPublication(jobId, {
        jobId,
        revision: expected.revision,
        accountId: expected.accountId,
        saveDate: request.saveDate,
    }, options, dependencies, { terminalFailure: 'throw' })
    if (!outcome) {
        throw new NativeFileJobError(
            'publication-failed',
            'Native official publication did not complete',
        )
    }
    return outcome
}

interface NativeOfficialPublicationExpectedResult {
    jobId: string
    revision?: number
    accountId?: string
    saveDate?: string
}

async function requestNativeOfficialPublicationCancellation(
    jobId: string,
    dependencies: NativeFileJobDependencies,
): Promise<void> {
    try {
        await invokeNative(dependencies, 'native_file_job_cancel', { jobId })
    }
    catch {}
}

async function pollNativeOfficialPublication(
    jobId: string,
    expected: NativeOfficialPublicationExpectedResult,
    options: NativeFileJobOptions,
    dependencies: NativeFileJobDependencies,
    behavior: {
        startWarningCodes?: readonly string[]
        cancelImmediately?: boolean
        cancelWhenWaiting?: boolean
        terminalFailure: 'throw' | 'return-null'
    },
): Promise<NativeOfficialPublicationRunResult | null> {
    let cancellationRequested = false
    if (behavior.cancelImmediately) {
        cancellationRequested = true
        await requestNativeOfficialPublicationCancellation(jobId, dependencies)
    }

    while (true) {
        if (options.signal?.aborted && !cancellationRequested) {
            cancellationRequested = true
            await requestNativeOfficialPublicationCancellation(jobId, dependencies)
        }
        const status = await invokeNative(dependencies, 'native_file_job_status', {
            jobId,
        }) as NativeFileJobStatus
        options.onStatus?.(status)
        if (status.jobId !== jobId || status.kind !== 'official-publication-upload') {
            throw new NativeFileJobError(
                'invalid-result',
                'Native official publication returned a mismatched job',
            )
        }
        if (
            status.state === 'waitingForInput'
            && status.phase === 'awaiting-publication-retry'
        ) {
            if (behavior.cancelWhenWaiting && !cancellationRequested) {
                cancellationRequested = true
                await requestNativeOfficialPublicationCancellation(jobId, dependencies)
            }
            if (!cancellationRequested) {
                const publication = assertOfficialPublicationResult(status.publicationAttempt)
                if (
                    publication.kind !== 'reauthentication-needed'
                    || publication.status !== 403
                    || (expected.accountId !== undefined
                        && publication.accountId !== expected.accountId)
                    || (expected.saveDate !== undefined
                        && publication.saveDate !== expected.saveDate)
                ) {
                    await requestNativeOfficialPublicationCancellation(jobId, dependencies)
                    throw new NativeFileJobError(
                        'invalid-result',
                        'Native official publication returned mismatched retry metadata',
                    )
                }
                return {
                    kind: 'waiting-for-reauthentication',
                    jobId,
                    accountId: publication.accountId,
                    session: publication.session,
                }
            }
        }
        if (status.state === 'succeeded') {
            return {
                kind: 'completed',
                receipt: createOfficialPublicationReceipt(
                    status,
                    dependencies,
                    expected,
                    behavior.startWarningCodes,
                ),
            }
        }
        if (status.state === 'failed' || status.state === 'cancelled') {
            const error = status.state === 'cancelled'
                ? abortError()
                : new NativeFileJobError(
                    status.error?.code ?? 'publication-failed',
                    status.error?.message ?? 'Native official publication failed',
                )
            let forgotten = false
            try {
                await invokeNative(dependencies, 'native_file_job_forget', { jobId })
                forgotten = true
            }
            catch {}
            if (behavior.terminalFailure === 'return-null') return null
            throw forgotten ? drainedNativeOfficialPublicationError(error) : error
        }
        await dependencies.wait(options.pollIntervalMs ?? 100)
    }
}

export async function cancelNativeOfficialPublication(
    jobId: string,
    options: NativeFileJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeOfficialPublicationReceipt | null> {
    if (!dependencies.isTauri()) {
        throw new Error('Native official publication requires Tauri')
    }
    const outcome = await pollNativeOfficialPublication(
        jobId,
        { jobId },
        options,
        dependencies,
        {
            cancelImmediately: true,
            terminalFailure: 'return-null',
        },
    )
    return outcome?.kind === 'completed' ? outcome.receipt : null
}

export async function resumeNativeOfficialPublication(
    jobId: string,
    options: NativeFileJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeOfficialPublicationReceipt | null> {
    if (!dependencies.isTauri()) {
        throw new Error('Native official publication requires Tauri')
    }
    const outcome = await pollNativeOfficialPublication(
        jobId,
        { jobId },
        options,
        dependencies,
        {
            cancelWhenWaiting: true,
            terminalFailure: 'return-null',
        },
    )
    return outcome?.kind === 'completed' ? outcome.receipt : null
}
