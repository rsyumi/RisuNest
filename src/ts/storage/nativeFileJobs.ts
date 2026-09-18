import { exportIOSFile } from './iosFiles'
import { MAX_CONTENT_METADATA_BYTES } from './contentImportLimits'
import { invoke } from '@tauri-apps/api/core'

import { isTauri } from '../platform'
import type { PreparedNativeCharacterCardModule } from '../characterCards'
import {
    finalizeContentCasJob,
    releaseCasJob,
    sealPreparedContentCasJob,
} from './nativeAssetRepository'
import type { PreparedImmutablePayload } from './payloadCas'
import type { DataHealthResult } from './dataHealth'
import type { PersistentDataRuntime } from './persistentDataRuntime'
import { registerCommittedWorkingSetContinuation } from './committedWorkingSetContinuation'
import {
    copyNativeExportToAndroidSaf,
    discardAndroidSafSource,
    type AndroidSafDestinationRequest,
    type AndroidSafDestinationResult,
} from './androidSafBridge'
import {
    forgetPluginValueAssignment,
    recallPluginValueAssignment,
    rememberPluginValueAssignment,
} from './pluginValueAssignmentRetention'
import type { NativePortableDeviceSection } from './deviceBackup/selection'

export type NativeFileJobSource =
    | { type: 'desktopPath'; path: string }
    | { type: 'androidSpool'; token: string }
    | { type: 'conflictReference'; token: string }

export type NativeFileJobState =
    | 'queued'
    | 'running'
    | 'waitingForInput'
    | 'cancelling'
    | 'succeeded'
    | 'failed'
    | 'cancelled'

export type NativeCompatibilityTarget = 'risuai' | 'pocketrisu'

export interface NativeCompatibilityReportItem {
    code: string
    items: string
    bytes: string
    affectedConversations: string | null
}

export interface NativeCompatibilityReport {
    target: NativeCompatibilityTarget
    preserved: NativeCompatibilityReportItem[]
    converted: NativeCompatibilityReportItem[]
    excluded: NativeCompatibilityReportItem[]
}

export interface NativeFileJobResult {
    revision: number
    sourceBytes: number
    sourceSha256: string
    characterCount: number
    presetCount: number
    warningCodes: string[]
    handoffPath?: string
    publication?: NativeOfficialPublicationAttemptResult
}

export interface NativeOfficialAccountSnapshotRestoreRequest {
    baseUrl: string
    credential:
        { kind: 'risu-auth'; token: string } | { kind: 'bearer'; token: string }
}

export type NativeOfficialAccountSnapshotRestoreResult =
    | { kind: 'missing' }
    | { kind: 'compatibility-fallback' }
    | {
          kind: 'activated'
          revision: number
          sourceSha256: string
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
    PreparedCardContentAssetDescriptor | PreparedRisumContentAssetDescriptor

export type PreparedRisumOwnerHead =
    | { present: false; manifestHash: null; entryCount: 0 }
    | { present: true; manifestHash: string; entryCount: number }

export interface PreparedNativeContent {
    casSessionId: string
    format:
        | 'json-card'
        | 'png-card'
        | 'charx-card'
        | 'appended-charx-jpeg'
        | 'risu-module'
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
    | (NativeOfficialPublicationCommonResult & { kind: 'auth-warning'; warning: string | null })
    | (NativeOfficialPublicationCommonResult & {
          kind: 'reauthentication-needed'
          warning: string | null
      })

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
          warning: string | null
          jobId: string
          accountId: string
          session: string | null
      }
    | {
          kind: 'completed'
          receipt: NativeOfficialPublicationReceipt
      }

/**
 * Sub-phase of a native file job. The first group mirrors the Rust
 * `JobStage` enum; the second group is synthesized by TypeScript routes for
 * work that happens outside the native job (Android SAF copies, renderer
 * refresh, plugin reload, WebView-path restarts, compatibility re-selection).
 */
export type NativeFileJobStage =
    | 'reading-archive'
    | 'preparing-attachments'
    | 'reading-database'
    | 'decoding-database'
    | 'staging-characters'
    | 'finalizing-staging'
    | 'assign-plugin-values'
    | 'awaiting-activation'
    | 'activating'
    | 'copying-source'
    | 'refreshing-app'
    | 'reloading-plugins'
    | 'restarting-app'
    | 'awaiting-reselect'

export interface NativeImportCounts {
    entriesRead: number
    entriesTotal?: number
    assets: number
    inlays: number
    coldStorage: number
    pocketMedia: number
    pocketMetadata: number
    skipped: number
    attachmentsPrepared: number
    characters: number
    charactersTotal?: number
    presets: number
    blocks: number
}

export interface NativeFileJobDetail {
    stage: NativeFileJobStage
    stageCompleted: number
    stageTotal?: number
    stageUnit: 'bytes' | 'items'
    currentItem?: string
    counts: NativeImportCounts
}

/** Which user-facing import a dialog-presented operation belongs to. */
export type NativeFileOperationFormat =
    | 'risu-save'
    | 'local-backup'
    | 'library-backup'
    | 'conflict-reference'
    | 'content'

/**
 * Resolves the stage a status describes. Statuses carrying `detail` name it
 * directly; older statuses (and jobs that never report detail) fall back to
 * the coarse job phase, which only knows the format-dependent first stage.
 */
export function resolveNativeFileJobStage(
    status: NativeFileJobStatus,
    format?: NativeFileOperationFormat,
): NativeFileJobStage | null {
    // The assignment pass happens inside the activation wait, so it is named
    // before the wait's own stage.
    if (status.phase === 'awaiting-activation' && status.pluginValuePreview?.values.length) {
        return 'assign-plugin-values'
    }
    if (status.detail) return status.detail.stage
    switch (status.phase) {
        case 'reading-source':
            // The common picker admits every backup as a library backup, so the
            // reported job kind, not the admission format, tells an archive apart.
            return status.kind === 'restore-legacy-local-backup' ||
                format === 'local-backup' ||
                format === 'content'
                ? 'reading-archive'
                : 'reading-database'
        case 'staging-database':
            return 'finalizing-staging'
        case 'awaiting-activation':
            return 'awaiting-activation'
        case 'activating-database':
            return 'activating'
        default:
            return null
    }
}

/**
 * Builds a status for work that happens outside the native job (renderer
 * refresh, plugin reload, WebView imports). It reuses the last native status
 * when there is one so counts and progress carry over into the new stage.
 */
export function syntheticNativeFileJobStatus(
    base: Pick<NativeFileJobStatus, 'kind'> & Partial<NativeFileJobStatus>,
    stage: NativeFileJobStage,
    patch: Partial<Omit<NativeFileJobDetail, 'stage' | 'counts'>> & {
        counts?: Partial<NativeImportCounts>
        progress?: Partial<NativeFileJobStatus['progress']>
        currentItem?: string
    } = {},
): NativeFileJobStatus {
    const { counts, progress, ...detailPatch } = patch
    return {
        jobId: base.jobId ?? `synthetic-${base.kind}`,
        kind: base.kind,
        state: base.state ?? 'running',
        phase: base.phase ?? 'reading-source',
        progress: {
            ...(base.progress ?? { completedBytes: 0, completedItems: 0 }),
            ...progress,
        },
        ...(base.warningCodes ? { warningCodes: base.warningCodes } : {}),
        ...(base.result ? { result: base.result } : {}),
        detail: {
            stageCompleted: 0,
            stageUnit: 'items',
            ...detailPatch,
            stage,
            counts: {
                ...(base.detail?.counts ?? emptyNativeImportCounts()),
                ...counts,
            },
        },
    }
}

export function emptyNativeImportCounts(): NativeImportCounts {
    return {
        entriesRead: 0,
        assets: 0,
        inlays: 0,
        coldStorage: 0,
        pocketMedia: 0,
        pocketMetadata: 0,
        skipped: 0,
        attachmentsPrepared: 0,
        characters: 0,
        presets: 0,
        blocks: 0,
    }
}

export interface NativeFileJobStatus {
    jobId: string
    kind:
        | 'restore-block-risu-save'
        | 'export-block-risu-save'
        | 'export-character-charx'
        | 'export-character-card'
        | 'export-risu-module'
        | 'restore-legacy-local-backup'
        | 'export-legacy-local-backup'
        | 'export-compatible-local-backup'
        | 'export-portable-backup'
        | 'restore-portable-backup'
        | 'prepare-content-import'
        | 'import-jpeg-asset'
        | 'kei-backup-upload'
        | 'restore-official-account-snapshot'
        | 'official-publication-upload'
    expectedRevision?: number
    deviceSessionId?: string
    restorePreview?: NativePortableRestorePreview
    pluginValuePreview?: NativeStagedPluginPreview
    warningCodes?: string[]
    state: NativeFileJobState
    phase:
        | 'queued'
        | 'reading-source'
        | 'awaiting-content-mapping'
        | 'awaiting-backup-selection'
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
    detail?: NativeFileJobDetail
    compatibilityReport?: NativeCompatibilityReport
    preservationReport?: {
        files: string
        bytes: string
        reason: 'not-required-by-library'
        deletable: boolean
        path: string
    }
    publicationAttempt?: NativeOfficialPublicationAttemptResult
    result?: NativeFileJobResult
    preparedContent?: PreparedNativeContent
    error?: {
        code: string
        message: string
    }
}

export interface NativePortableSelection {
    library: boolean
    deviceSections: NativePortableDeviceSection[]
    /** Absent brings the whole library; present brings only the records it names. */
    items?: NativeArchiveSelection
}
/** One record an import can take or leave. */
export interface NativeArchiveEntry {
    id: string
    conversations: number
    damaged: number
}
export interface NativeArchiveInventory {
    characters: NativeArchiveEntry[]
    presets: NativeArchiveEntry[]
    plugins: NativeArchiveEntry[]
}
export interface NativeArchiveSelection {
    characters: string[]
    presets: string[]
    plugins: string[]
    /** Records left out on purpose, whose references come in broken. */
    excluded: string[]
}
export interface NativePortableRestorePreview {
    libraryIncluded: boolean
    repairRequired: boolean
    deviceSections: NativePortableDeviceSection[]
    /** What is wrong with the archive's library, even when the gate refuses it. */
    diagnosis?: DataHealthResult
    items?: NativeArchiveInventory
}

export type NativeBlockRestoreRuntime = Pick<
    PersistentDataRuntime,
    | 'capturePersistentMutationToken'
    | 'acquireDestructiveReplacementFence'
    | 'markCommittedWorkingSetRefreshRequired'
    | 'getStorageAuthorityEpoch'
>

export interface NativeFileJobOptions {
    onStarted?(jobId: string): void | Promise<void>
    onNativeStatus?(status: NativeFileJobStatus): void | Promise<void>
    signal?: AbortSignal
    pollIntervalMs?: number
    onStatus?(status: NativeFileJobStatus): void
}

export interface PreparedNativeContentActivationLifecycle {
    prepareOwnerManifestAndSeal(
        bytes: Uint8Array,
    ): Promise<PreparedImmutablePayload>
    sealPreparedContent?(): Promise<void>
    abortPreparedContent?(): Promise<void>
}

export interface PreparedNativeContentReceipt extends PreparedNativeContentActivationLifecycle {
    readonly jobId: string
    readonly content: PreparedNativeContent
    readonly warningCodes: string[]
    confirmActivated(): Promise<void>
    cancel(): Promise<void>
}

/** One staged value a save left without an owner. Sizes only, never values. */
export interface NativeStagedPluginValue {
    key: string
    byteSize: number
    valueType: 'json' | 'string'
}

/**
 * What the one assignment pass is offered. The plugin names come from the
 * staged save, because the working set still holds the database it replaces.
 */
export interface NativeStagedPluginPreview {
    values: NativeStagedPluginValue[]
    pluginNames: string[]
}

export interface NativeStagedPluginAssignment {
    owner: string
    keys: string[]
}

export interface NativeStagedPluginChoice {
    assignments: NativeStagedPluginAssignment[]
    /** Whether the values left here may go to the first plugin that asks. */
    automatic: boolean
}

export interface NativeFileRestoreJobOptions extends NativeFileJobOptions {
    /** Runs after staging and before taking the destructive replacement fence. */
    beforeActivation?(): void | Promise<void>
    /**
     * Asks who owns the plugin values a save left unassigned. Answering with
     * nothing cancels the import, which is what a person closing the pass means.
     * The answers an earlier attempt over the same save gave arrive with it, so
     * the pass opens on them instead of on nothing.
     */
    assignPluginValues?(
        preview: NativeStagedPluginPreview,
        remembered: NativeStagedPluginChoice | null,
    ): Promise<NativeStagedPluginChoice | null>
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
    container?: 'appended-charx-jpeg'
}

export interface NativeCharacterCardExportInput {
    characterId: string
    destination: NativeCharacterCharxExportDestination
    expectedRevision: number
    format: 'json-card' | 'png-card'
    metadata: Record<string, unknown>
}

export interface NativeRisuModuleExportInput {
    moduleIndex: number
    destination: NativeCharacterCharxExportDestination
    expectedRevision: number
}

export type NativeCharacterCharxExportDestination =
    | { type: 'desktopPath'; path: string }
    | { type: 'androidSaf'; suggestedName: string }
    | { type: 'iosFiles'; suggestedName: string }

export type NativeBackupDestination =
    | { type: 'desktopPath'; path: string }
    | { type: 'androidSaf'; suggestedName: string }
    | { type: 'iosFiles'; suggestedName: string }

export type NativeLegacyLocalBackupDestination = NativeBackupDestination

export interface NativeFileJobDependencies {
    isTauri(): boolean
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    wait(milliseconds: number): Promise<void>
    discardAndroidSource?(token: string): boolean
}

export interface NativeBackupExportDependencies extends NativeFileJobDependencies {
    copyToIOSFiles?: typeof exportIOSFile
    copyToAndroidSaf(
        request: AndroidSafDestinationRequest,
    ): Promise<AndroidSafDestinationResult>
}

const productionDependencies: NativeFileJobDependencies = {
    isTauri: () => isTauri,
    invoke: (command, args) =>
        args === undefined ? invoke(command) : invoke(command, args),
    wait: (milliseconds) =>
        new Promise((resolve) => setTimeout(resolve, milliseconds)),
    discardAndroidSource: (token) => discardAndroidSafSource(token),
}

const productionBackupExportDependencies: NativeBackupExportDependencies = {
    ...productionDependencies,
    copyToIOSFiles: exportIOSFile,
    copyToAndroidSaf: (request) => copyNativeExportToAndroidSaf(request),
}

export class NativeFileJobError extends Error {
    constructor(
        readonly code: string,
        message: string,
        readonly warningCodes: string[] = [],
    ) {
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

export function isTerminalJob(status: NativeFileJobStatus): boolean {
    return (
        status.state === 'succeeded' ||
        status.state === 'failed' ||
        status.state === 'cancelled'
    )
}

function mergeWarningCodes(
    ...groups: ReadonlyArray<readonly string[] | undefined>
): string[] {
    return [...new Set(groups.flatMap((group) => group ?? []))].slice(0, 16)
}

function withCleanupFailedWarning(codes: readonly string[]): string[] {
    return [
        ...codes.filter((code) => code !== 'cleanup-failed').slice(0, 15),
        'cleanup-failed',
    ]
}

function preparedContentError(message: string): NativeFileJobError {
    return new NativeFileJobError('invalid-prepared-content', message)
}

function requiredDescriptorString(value: unknown, field: string): string {
    if (typeof value !== 'string' || value.length === 0) {
        throw preparedContentError(
            `Prepared content asset ${field} must be a nonempty string`,
        )
    }
    return value
}

function validatePreparedContent(
    value: unknown,
    expectedCasSessionId: string,
): PreparedNativeContent {
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
        throw preparedContentError('Prepared content must be an object')
    }
    const content = value as Record<string, unknown>
    if (
        content.format !== 'json-card' &&
        content.format !== 'png-card' &&
        content.format !== 'charx-card' &&
        content.format !== 'appended-charx-jpeg' &&
        content.format !== 'risu-module'
    ) {
        throw preparedContentError('Prepared content format is unsupported')
    }
    if (
        typeof content.metadata !== 'object' ||
        content.metadata === null ||
        Array.isArray(content.metadata)
    ) {
        throw preparedContentError(
            'Prepared content metadata must be an object',
        )
    }
    if (!Array.isArray(content.assets)) {
        throw preparedContentError('Prepared content assets must be an array')
    }
    const casSessionId = requiredDescriptorString(
        content.casSessionId,
        'casSessionId',
    )
    if (casSessionId !== expectedCasSessionId) {
        throw preparedContentError(
            'Prepared content casSessionId must match its native job',
        )
    }
    const expectedContentFields = [
        'assets',
        'casSessionId',
        'format',
        'metadata',
    ]
    if (content.portraitLogicalId !== undefined)
        expectedContentFields.push('portraitLogicalId')
    if (content.module !== undefined) expectedContentFields.push('module')
    if (content.ownerHead !== undefined) expectedContentFields.push('ownerHead')
    if (
        Object.keys(content).sort().join('\0') !==
        expectedContentFields.sort().join('\0')
    ) {
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
    const assets = content.assets.map(
        (value, index): PreparedContentAssetDescriptor => {
            if (
                typeof value !== 'object' ||
                value === null ||
                Array.isArray(value)
            ) {
                throw preparedContentError(
                    `Prepared content asset ${index} must be an object`,
                )
            }
            const asset = value as Record<string, unknown>
            const risum = content.format === 'risu-module'
            const expectedFields = risum
                ? expectedRisumFields
                : expectedCardFields
            if (
                Object.keys(asset).sort().join('\0') !==
                expectedFields.join('\0')
            ) {
                throw preparedContentError(
                    `Prepared content asset ${index} fields are invalid`,
                )
            }
            const objectHash = requiredDescriptorString(
                asset.objectHash,
                'objectHash',
            )
            if (!/^[0-9a-f]{64}$/.test(objectHash)) {
                throw preparedContentError(
                    `Prepared content asset ${index} objectHash is invalid`,
                )
            }
            if (
                !Number.isSafeInteger(asset.byteSize) ||
                (asset.byteSize as number) < 0
            ) {
                throw preparedContentError(
                    `Prepared content asset ${index} byteSize is invalid`,
                )
            }
            let token: string | undefined
            if (!risum) {
                token = requiredDescriptorString(asset.token, 'token')
                if (tokens.has(token)) {
                    throw preparedContentError(
                        `Prepared content asset ${index} token is duplicated`,
                    )
                }
                tokens.add(token)
            }
            const ext =
                risum && typeof asset.ext === 'string'
                    ? asset.ext
                    : requiredDescriptorString(asset.ext, 'ext')
            if (!risum && (ext.length > 32 || !/^[A-Za-z0-9+_-]+$/.test(ext))) {
                throw preparedContentError(
                    `Prepared content asset ${index} ext is invalid`,
                )
            }
            const logicalId = requiredDescriptorString(
                asset.logicalId,
                'logicalId',
            )
            const logicalPrefix = `assets/${objectHash}.`
            if (!logicalId.startsWith(logicalPrefix)) {
                throw preparedContentError(
                    `Prepared content asset ${index} logicalId does not match its object`,
                )
            }
            const logicalSuffix = logicalId.slice(logicalPrefix.length)
            if (
                logicalSuffix.length > 32 ||
                !/^[A-Za-z0-9+_-]+$/.test(logicalSuffix)
            ) {
                throw preparedContentError(
                    `Prepared content asset ${index} logicalId suffix is invalid`,
                )
            }
            if (typeof asset.name !== 'string') {
                throw preparedContentError(
                    `Prepared content asset ${index} name must be a string`,
                )
            }
            if (typeof asset.mime !== 'string') {
                throw preparedContentError(
                    `Prepared content asset ${index} mime must be a string`,
                )
            }
            const mime = asset.mime
            if (mime.length === 0 && content.format !== 'png-card' && !risum) {
                throw preparedContentError(
                    `Prepared content asset ${index} mime must be a nonempty string`,
                )
            }
            if (risum) {
                if (
                    !Number.isSafeInteger(asset.position) ||
                    asset.position !== index
                ) {
                    throw preparedContentError(
                        `Prepared RISUM asset ${index} position is invalid`,
                    )
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
                referenceKey: requiredDescriptorString(
                    asset.referenceKey,
                    'referenceKey',
                ),
                token: token!,
                logicalId,
                objectHash,
                byteSize: asset.byteSize as number,
                mime,
                name: asset.name,
                ext,
            }
        },
    )
    if (content.format === 'risu-module') {
        if (
            content.portraitLogicalId !== undefined ||
            content.module !== undefined
        ) {
            throw preparedContentError(
                'Prepared RISUM content cannot contain card fields',
            )
        }
        if (
            typeof content.ownerHead !== 'object' ||
            content.ownerHead === null ||
            Array.isArray(content.ownerHead)
        ) {
            throw preparedContentError(
                'Prepared RISUM ownerHead must be an object',
            )
        }
        const ownerHead = content.ownerHead as Record<string, unknown>
        if (
            Object.keys(ownerHead).sort().join('\0') !==
            ['entryCount', 'manifestHash', 'present'].join('\0')
        ) {
            throw preparedContentError(
                'Prepared RISUM ownerHead fields are invalid',
            )
        }
        if (typeof ownerHead.present !== 'boolean') {
            throw preparedContentError(
                'Prepared RISUM ownerHead present is invalid',
            )
        }
        if (
            !Number.isSafeInteger(ownerHead.entryCount) ||
            (ownerHead.entryCount as number) < 0
        ) {
            throw preparedContentError(
                'Prepared RISUM ownerHead entryCount is invalid',
            )
        }
        if (ownerHead.present) {
            if (
                typeof ownerHead.manifestHash !== 'string' ||
                !/^[0-9a-f]{64}$/.test(ownerHead.manifestHash)
            ) {
                throw preparedContentError(
                    'Prepared RISUM ownerHead manifestHash is invalid',
                )
            }
            if (ownerHead.entryCount !== assets.length) {
                throw preparedContentError(
                    'Prepared RISUM ownerHead entryCount does not match assets',
                )
            }
        } else if (
            ownerHead.manifestHash !== null ||
            ownerHead.entryCount !== 0 ||
            assets.length !== 0
        ) {
            throw preparedContentError(
                'Absent RISUM ownerHead must have no assets',
            )
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
        portraitLogicalId = requiredDescriptorString(
            content.portraitLogicalId,
            'portraitLogicalId',
        )
        if (
            !cardAssets.some((asset) => asset.logicalId === portraitLogicalId)
        ) {
            throw preparedContentError(
                'Prepared content portraitLogicalId must reference a prepared asset',
            )
        }
    }
    if (content.format === 'png-card') {
        const metadata = content.metadata as Record<string, unknown>
        if (
            !Object.keys(metadata).every(
                (field) => field === 'chara' || field === 'ccv3',
            )
        ) {
            throw preparedContentError(
                'Prepared PNG metadata fields are invalid',
            )
        }
        const encodedMetadata = [metadata.chara, metadata.ccv3]
        if (
            !encodedMetadata.some(
                (value) => typeof value === 'string' && value.length > 0,
            )
        ) {
            throw preparedContentError('Prepared PNG card metadata is missing')
        }
        for (const value of encodedMetadata) {
            if (
                value !== undefined &&
                (typeof value !== 'string' || value.length === 0)
            ) {
                throw preparedContentError(
                    'Prepared PNG card metadata must be a nonempty string',
                )
            }
            if (
                typeof value === 'string' &&
                value.length > MAX_CONTENT_METADATA_BYTES
            ) {
                throw preparedContentError(
                    'Prepared PNG card metadata exceeds the 128 MiB limit',
                )
            }
        }
        if (content.module !== undefined) {
            throw preparedContentError(
                'Prepared PNG content cannot contain a module',
            )
        }
        if (!portraitLogicalId) {
            throw preparedContentError(
                'Prepared PNG portraitLogicalId is required',
            )
        }
        const portrait = cardAssets[0]
        if (!portrait || portrait.logicalId !== portraitLogicalId) {
            throw preparedContentError(
                'Prepared PNG portrait must be the first asset',
            )
        }
        if (
            !/^native-png-portrait(?:-[1-9][0-9]*)?$/.test(portrait.token) ||
            portrait.referenceKey !== portrait.token ||
            portrait.mime !== 'image/png' ||
            portrait.ext !== 'png' ||
            portrait.logicalId !== `assets/${portrait.objectHash}.png` ||
            portrait.name !== `${portrait.objectHash}.png`
        ) {
            throw preparedContentError(
                'Prepared PNG portrait descriptor is invalid',
            )
        }
        for (const [index, asset] of cardAssets.slice(1).entries()) {
            if (asset.token !== asset.referenceKey) {
                throw preparedContentError(
                    `Prepared PNG embedded asset ${index} token must equal referenceKey`,
                )
            }
            if (asset.mime !== '') {
                throw preparedContentError(
                    `Prepared PNG embedded asset ${index} mime must be empty`,
                )
            }
            if (
                asset.ext !== 'png' ||
                asset.logicalId !== `assets/${asset.objectHash}.png` ||
                asset.name !== `${asset.objectHash}.png`
            ) {
                throw preparedContentError(
                    `Prepared PNG embedded asset ${index} descriptor is invalid`,
                )
            }
        }
    }
    let module: PreparedNativeCharacterCardModule | undefined
    if (content.module !== undefined) {
        if (
            typeof content.module !== 'object' ||
            content.module === null ||
            Array.isArray(content.module)
        ) {
            throw preparedContentError(
                'Prepared content module must be an object',
            )
        }
        const rawModule = content.module as Record<string, unknown>
        const expectedModuleFields = ['lorebook', 'regex', 'trigger']
        if (
            !Object.keys(rawModule).every((field) =>
                expectedModuleFields.includes(field),
            )
        ) {
            throw preparedContentError(
                'Prepared content module fields are invalid',
            )
        }
        for (const field of expectedModuleFields) {
            if (
                rawModule[field] !== undefined &&
                !Array.isArray(rawModule[field])
            ) {
                throw preparedContentError(
                    `Prepared content module ${field} must be an array`,
                )
            }
        }
        module = {
            ...(rawModule.trigger === undefined
                ? {}
                : {
                      trigger:
                          rawModule.trigger as PreparedNativeCharacterCardModule['trigger'],
                  }),
            ...(rawModule.regex === undefined
                ? {}
                : {
                      regex: rawModule.regex as PreparedNativeCharacterCardModule['regex'],
                  }),
            ...(rawModule.lorebook === undefined
                ? {}
                : {
                      lorebook:
                          rawModule.lorebook as PreparedNativeCharacterCardModule['lorebook'],
                  }),
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
    } catch (error) {
        if (
            typeof error === 'object' &&
            error !== null &&
            'code' in error &&
            typeof error.code === 'string' &&
            'message' in error &&
            typeof error.message === 'string'
        ) {
            throw new NativeFileJobError(error.code, error.message)
        }
        throw error
    }
}

async function pollNativeFileJobUntilTerminal(
    jobId: string,
    options: NativeFileJobOptions,
    dependencies: NativeFileJobDependencies,
): Promise<NativeFileJobStatus> {
    let cancellationRequested = false
    while (true) {
        if (options.signal?.aborted && !cancellationRequested) {
            cancellationRequested = true
            await invokeNative(dependencies, 'native_file_job_cancel', {
                jobId,
            })
        }
        const status = (await invokeNative(
            dependencies,
            'native_file_job_status',
            {
                jobId,
            },
        )) as NativeFileJobStatus
        options.onStatus?.(status)
        if (isTerminalJob(status)) return status
        await options.onNativeStatus?.(status)
        await dependencies.wait(options.pollIntervalMs ?? 100)
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
            discarded =
                dependencies.discardAndroidSource?.(source.token) === true
        } catch {}
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
        | 'restore-portable-backup'
        | 'restore-legacy-local-backup'
        | 'restore-official-account-snapshot',
    mutationReason: string,
    runtime: NativeBlockRestoreRuntime,
    source: NativeFileJobSource | NativeOfficialAccountSnapshotRestoreRequest,
    options: NativeFileRestoreJobOptions & {
        choosePortableSections?(
            preview: NativePortableRestorePreview,
        ): Promise<NativePortableSelection | null>
    } = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    const operation =
        kind === 'restore-official-account-snapshot'
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
        mutationReason, { publishOfficial: false },
    )
    if (options.signal?.aborted) {
        abortBeforeNativeRestoreStart(source, dependencies)
    }
    const request =
        kind === 'restore-official-account-snapshot'
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
    const started = (await invokeNative(dependencies, 'native_file_job_start', {
        request,
    })) as {
        jobId: string
        warningCodes?: string[]
    }
    let cancellationRequested = false
    let uiBlocking = false
    let portableSelectionMade =
        kind !== 'restore-portable-backup' ||
        ('type' in source && source.type === 'conflictReference')
    let terminal: NativeFileJobStatus | undefined
    let mutationConflict: NativeFileJobError | undefined
    let assignedPreview: NativeStagedPluginPreview | undefined
    let replacementFence:
        | Awaited<
              ReturnType<
                  NativeBlockRestoreRuntime['acquireDestructiveReplacementFence']
              >
          >
        | undefined
    const startUiBlocking = (): void => {
        if (uiBlocking) return
        uiBlocking = true
        options.onBlockingChange?.(true)
    }
    try {
        while (!terminal) {
            if (options.signal?.aborted && !cancellationRequested) {
                cancellationRequested = true
                await invokeNative(dependencies, 'native_file_job_cancel', {
                    jobId: started.jobId,
                })
            }
            const status = (await invokeNative(
                dependencies,
                'native_file_job_status',
                {
                    jobId: started.jobId,
                },
            )) as NativeFileJobStatus
            if (['succeeded', 'failed', 'cancelled'].includes(status.state))
                terminal = status
            try {
                await options.onNativeStatus?.(status)
                options.onStatus?.(status)
            } catch (error) {
                if (status.state === 'succeeded' && status.result)
                    throw new NativeFileJobActivationCommittedError(
                        status.result.revision,
                        error,
                    )
                throw error
            }
            if (
                portableSelectionMade &&
                status.state === 'running' &&
                !cancellationRequested
            ) {
                startUiBlocking()
            }
            if (
                kind === 'restore-portable-backup' &&
                status.state === 'waitingForInput' &&
                status.phase === 'awaiting-backup-selection'
            ) {
                if (!status.restorePreview || !options.choosePortableSections)
                    throw new NativeFileJobError(
                        'selection-unavailable',
                        'Backup section selection is unavailable',
                    )
                const selection = await options.choosePortableSections(
                    status.restorePreview,
                )
                if (!selection || options.signal?.aborted) {
                    cancellationRequested = true
                    await invokeNative(dependencies, 'native_file_job_cancel', {
                        jobId: started.jobId,
                    })
                    continue
                }
                if (options.signal?.aborted) {
                    cancellationRequested = true
                    await invokeNative(dependencies, 'native_file_job_cancel', {
                        jobId: started.jobId,
                    })
                    continue
                }
                await invokeNative(
                    dependencies,
                    'native_portable_select_sections',
                    {
                        jobId: started.jobId,
                        selection,
                    },
                )
                portableSelectionMade = true
                startUiBlocking()
            }
            if (
                status.state === 'waitingForInput' &&
                status.phase === 'awaiting-activation' &&
                !replacementFence &&
                !cancellationRequested &&
                status.pluginValuePreview?.values.length &&
                options.assignPluginValues
            ) {
                const preview = status.pluginValuePreview
                const choice = await options.assignPluginValues(
                    preview,
                    recallPluginValueAssignment(preview),
                )
                // Held before anything else can go wrong, because losing the
                // replacement fence past this point cancels the job and the
                // person answers the same pass again on the next attempt.
                if (choice) {
                    rememberPluginValueAssignment(preview, choice)
                    assignedPreview = preview
                }
                if (!choice || options.signal?.aborted) {
                    cancellationRequested = true
                    await invokeNative(dependencies, 'native_file_job_cancel', {
                        jobId: started.jobId,
                    })
                    continue
                }
                await invokeNative(dependencies, 'native_plugin_values_assign', {
                    jobId: started.jobId,
                    assignments: choice.assignments,
                    automatic: choice.automatic,
                })
            }
            if (
                status.state === 'waitingForInput' &&
                status.phase === 'awaiting-activation' &&
                !replacementFence &&
                !cancellationRequested
            ) {
                try {
                    startUiBlocking()
                    await options.beforeActivation?.()
                    // Explicit restores accept a fresh token after preparation and choices.
                    const activationToken = await runtime.capturePersistentMutationToken(
                        `${mutationReason}-activation`, { publishOfficial: false },
                    )
                    replacementFence = await runtime.acquireDestructiveReplacementFence(activationToken)
                } catch (error) {
                    mutationConflict =
                        error instanceof NativeFileJobError
                            ? error
                            : new NativeFileJobError(
                                  'revision-conflict',
                                  error instanceof Error
                                      ? error.message
                                      : String(error),
                              )
                    cancellationRequested = true
                    await invokeNative(dependencies, 'native_file_job_cancel', {
                        jobId: started.jobId,
                    })
                    continue
                }
                if (options.signal?.aborted) {
                    cancellationRequested = true
                    await invokeNative(dependencies, 'native_file_job_cancel', {
                        jobId: started.jobId,
                    })
                    continue
                }
                // Finalization uses the exact token captured after preparation and choices.
                await invokeNative(dependencies, 'native_file_job_finalize', {
                    jobId: started.jobId,
                    expectedRevision: replacementFence.revision,
                })
                options.onStatus?.({
                    ...status,
                    state: 'running',
                    phase: 'activating-database',
                })
            }
            if (
                status.state === 'succeeded' ||
                status.state === 'failed' ||
                status.state === 'cancelled'
            ) {
                terminal = status
                break
            }
            await dependencies.wait(options.pollIntervalMs ?? 100)
        }

        if (terminal.state === 'succeeded') {
            if (assignedPreview) forgetPluginValueAssignment(assignedPreview)
            if (!terminal.result) {
                throw new NativeFileJobError(
                    'missing-result',
                    `${operation} returned no result`,
                )
            }
            if (!replacementFence) {
                throw new NativeFileJobError(
                    'missing-activation-fence',
                    'Native restore committed without a renderer replacement fence',
                )
            }
            const committedResult = {
                ...terminal.result,
                warningCodes: mergeWarningCodes(
                    started.warningCodes,
                    terminal.result.warningCodes,
                ),
            }
            const continueAfterRefresh = async (): Promise<void> => {
                options.onStatus?.(
                    syntheticNativeFileJobStatus(terminal, 'reloading-plugins'),
                )
                let followupFailed = false
                let followupError: unknown
                try {
                    await options.afterRefresh?.()
                } catch (error) {
                    followupFailed = true
                    followupError = error
                }
                try {
                    await invokeNative(dependencies, 'native_file_job_forget', {
                        jobId: started.jobId,
                    })
                } catch {
                    committedResult.warningCodes = withCleanupFailedWarning(
                        committedResult.warningCodes,
                    )
                }
                if (followupFailed) throw followupError
            }
            const deviceSessionId = terminal.deviceSessionId
            if (kind === 'restore-portable-backup' && deviceSessionId) {
                let recoveryAcknowledged = false
                const prepareCommittedRefresh = async (): Promise<void> => {
                    if (!recoveryAcknowledged) {
                        await invokeNative(
                            dependencies,
                            'native_device_backup_recovery_complete',
                            { sessionId: deviceSessionId },
                        )
                        recoveryAcknowledged = true
                    }
                    const opened = (await invokeNative(
                        dependencies,
                        'pds_open',
                    )) as { revision?: unknown }
                    if (
                        !Number.isSafeInteger(opened?.revision) ||
                        (opened.revision as number) < terminal.result.revision
                    ) {
                        throw new NativeFileJobError(
                            'revision-conflict',
                            'Persistent store reopened before the committed native restore revision',
                        )
                    }
                }
                try {
                    await prepareCommittedRefresh()
                } catch (error) {
                    const committedError = new NativeFileJobActivationCommittedError(
                        terminal.result.revision,
                        error,
                    )
                    runtime.markCommittedWorkingSetRefreshRequired(
                        terminal.result.revision,
                        committedError,
                    )
                    registerCommittedWorkingSetContinuation(
                        terminal.result.revision,
                        runtime,
                        runtime.getStorageAuthorityEpoch(),
                        continueAfterRefresh,
                        prepareCommittedRefresh,
                    )
                    throw committedError
                }
            }
            try {
                options.onStatus?.(
                    syntheticNativeFileJobStatus(terminal, 'refreshing-app'),
                )
                const outcome = await replacementFence.refreshCommittedWorkingSet(
                    terminal.result.revision,
                )
                replacementFence.release()
                replacementFence = undefined
                if (outcome.projection === 'refresh-required') {
                    registerCommittedWorkingSetContinuation(
                        terminal.result.revision,
                        runtime,
                        runtime.getStorageAuthorityEpoch(),
                        continueAfterRefresh,
                    )
                    throw new Error('Committed native restore requires a read-only working-set refresh')
                }
                await continueAfterRefresh()
            } catch (error) {
                throw new NativeFileJobActivationCommittedError(
                    terminal.result.revision,
                    error,
                )
            }
            return committedResult
        }

        const error =
            mutationConflict ??
            (terminal.state === 'cancelled'
                ? abortError()
                : new NativeFileJobError(
                      terminal.error?.code ?? 'restore-failed',
                      terminal.error?.message ?? `${operation} failed`,
                  ))
        try {
            await invokeNative(dependencies, 'native_file_job_forget', {
                jobId: started.jobId,
            })
        } catch {}
        throw error
    } catch (error) {
        if (!terminal) {
            try {
                await invokeNative(dependencies, 'native_file_job_cancel', {
                    jobId: started.jobId,
                })
            } catch {}
        }
        throw error
    } finally {
        replacementFence?.release()
        if (uiBlocking) options.onBlockingChange?.(false)
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


export function runNativeArchiveRestore(
    runtime: NativeBlockRestoreRuntime,
    source: NativeFileJobSource,
    options: NativeFileRestoreJobOptions & {
        choosePortableSections(
            preview: NativePortableRestorePreview,
        ): Promise<NativePortableSelection | null>
    },
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    return runNativeReplacementRestore(
        'restore-portable-backup',
        'native-portable-restore',
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
    } catch (error) {
        if (
            error instanceof NativeFileJobError &&
            error.code === 'remote-missing'
        ) {
            return { kind: 'missing' }
        }
        if (
            error instanceof NativeFileJobError &&
            error.code === 'compatibility-required'
        ) {
            return { kind: 'compatibility-fallback' }
        }
        throw error
    }
    return {
        kind: 'activated',
        revision: result.revision,
        sourceSha256: result.sourceSha256,
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
    const started = (await invokeNative(dependencies, 'native_file_job_start', {
        request: {
            kind: 'export-block-risu-save',
            destination,
            expectedRevision,
            omitAccount: options.omitAccount ?? false,
        },
    })) as { jobId: string; warningCodes?: string[] }
    const terminal = await pollNativeFileJobUntilTerminal(
        started.jobId,
        options,
        dependencies,
    )

    let outcomeFailed = false
    let committedResult: NativeFileJobResult | undefined
    try {
        if (terminal.state === 'succeeded') {
            if (!terminal.result) {
                throw new NativeFileJobError(
                    'missing-result',
                    'Native export returned no result',
                )
            }
            committedResult = {
                ...terminal.result,
                warningCodes: mergeWarningCodes(
                    started.warningCodes,
                    terminal.result.warningCodes,
                ),
            }
            return committedResult
        }

        if (terminal.state === 'cancelled') throw abortError()
        throw new NativeFileJobError(
            terminal.error?.code ?? 'export-failed',
            terminal.error?.message ?? 'Native block RisuSave export failed',
        )
    } catch (error) {
        outcomeFailed = true
        throw error
    } finally {
        try {
            await invokeNative(dependencies, 'native_file_job_forget', {
                jobId: started.jobId,
            })
        } catch (error) {
            if (committedResult) {
                committedResult.warningCodes = withCleanupFailedWarning(
                    committedResult.warningCodes,
                )
            } else if (!outcomeFailed) throw error
        }
    }
}

interface NativeManagedExportSpec {
    operation: string
    safLengthMismatchLabel: string
    handoffCleanupCommand: string
    destination: NativeCharacterCharxExportDestination
    relaySafCopyProgress?: boolean
    prepareRequest(): Record<string, unknown> | Promise<Record<string, unknown>>
}

/**
 * Shared start/poll/terminal/SAF-handoff/cleanup skeleton for every managed
 * native export kind. When the Android handoff cleanup fails, the native job
 * is intentionally retained (fail closed) so bootstrap recovery can retry the
 * cleanup before forgetting the job.
 */
async function runNativeManagedExport(
    spec: NativeManagedExportSpec,
    options: NativeFileJobOptions,
    dependencies: NativeBackupExportDependencies,
): Promise<NativeFileJobResult> {
    if (!dependencies.isTauri()) {
        throw new Error(`${spec.operation} requires Tauri`)
    }
    if (options.signal?.aborted) throw abortError()
    const request = await spec.prepareRequest()
    const started = (await invokeNative(dependencies, 'native_file_job_start', {
        request,
    })) as { jobId: string; warningCodes?: string[] }
    let terminal: NativeFileJobStatus
    try {
        await options.onStarted?.(started.jobId)
        terminal = await pollNativeFileJobUntilTerminal(
            started.jobId,
            options,
            dependencies,
        )
    } catch (error) {
        try {
            await invokeNative(dependencies, 'native_file_job_cancel', {
                jobId: started.jobId,
            })
        } catch {}
        throw error
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
                terminal.error?.message ?? `${spec.operation} failed`,
            )
        }
        if (!terminal.result) {
            throw new NativeFileJobError(
                'missing-result',
                `${spec.operation} returned no result`,
            )
        }
        result = {
            ...terminal.result,
            warningCodes: mergeWarningCodes(
                started.warningCodes,
                terminal.result.warningCodes,
            ),
        }
        if (
            spec.destination.type === 'androidSaf' ||
            spec.destination.type === 'iosFiles'
        ) {
            managedSource = result.handoffPath
            if (!managedSource) {
                throw new NativeFileJobError(
                    'missing-handoff',
                    `${spec.operation} returned no native handoff path`,
                )
            }
            const committedResult = result
            const publish =
                spec.destination.type === 'iosFiles'
                    ? dependencies.copyToIOSFiles
                    : dependencies.copyToAndroidSaf
            if (!publish)
                throw new NativeFileJobError(
                    'unsupported',
                    'iOS file publication is unavailable',
                )
            const published = await publish({
                sourcePath: managedSource,
                suggestedName: spec.destination.suggestedName,
                signal: options.signal,
                ...(spec.relaySafCopyProgress
                    ? {
                          onProgress: (progress) =>
                              options.onStatus?.({
                                  ...terminal,
                                  state: 'running',
                                  phase: 'publishing-destination',
                                  progress: {
                                      completedBytes: progress.copiedBytes,
                                      ...(progress.totalBytes === null
                                          ? {
                                                totalBytes:
                                                    committedResult.sourceBytes,
                                            }
                                          : {
                                                totalBytes: progress.totalBytes,
                                            }),
                                      completedItems: 0,
                                      totalItems: 1,
                                  },
                              }),
                      }
                    : {}),
            })
            if (published.bytes !== result.sourceBytes) {
                throw new NativeFileJobError(
                    'length-mismatch',
                    `Published ${spec.safLengthMismatchLabel} length differs from its native source`,
                    mergeWarningCodes(
                        result.warningCodes,
                        published.warningCodes,
                        ['partial-destination-may-remain'],
                    ),
                )
            }
            const { handoffPath: _handoffPath, ...publishedResult } = result
            result = {
                ...publishedResult,
                warningCodes: mergeWarningCodes(
                    publishedResult.warningCodes,
                    published.warningCodes,
                ),
            }
        }
        return result
    } catch (error) {
        outcomeFailed = true
        throw error
    } finally {
        if (managedSource) {
            try {
                await invokeNative(dependencies, spec.handoffCleanupCommand, {
                    path: managedSource,
                })
            } catch {
                handoffCleanupFailed = true
                if (result && !outcomeFailed) {
                    result.warningCodes = withCleanupFailedWarning(
                        result.warningCodes,
                    )
                }
            }
        }
        if (!handoffCleanupFailed) {
            try {
                await invokeNative(dependencies, 'native_file_job_forget', {
                    jobId: started.jobId,
                })
            } catch (error) {
                if (result && !outcomeFailed) {
                    result.warningCodes = withCleanupFailedWarning(
                        result.warningCodes,
                    )
                } else if (!outcomeFailed) throw error
            }
        }
    }
}

export function runNativeCharacterCharxExport(
    input: NativeCharacterCharxExportInput,
    options: NativeFileJobOptions = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
): Promise<NativeFileJobResult> {
    return runNativeManagedExport(
        {
            operation: 'Native character CharX export',
            safLengthMismatchLabel: 'character CharX',
            handoffCleanupCommand: 'native_character_charx_handoff_cleanup',
            destination: input.destination,
            prepareRequest: () => ({
                kind: 'export-character-charx',
                ...(input.destination.type === 'desktopPath'
                    ? { destination: input.destination.path }
                    : {}),
                expectedRevision: input.expectedRevision,
                characterId: input.characterId,
                ...(input.container ? { container: input.container } : {}),
                card: input.card,
                module: input.module,
            }),
        },
        options,
        dependencies,
    )
}

export function runNativeCharacterCardExport(
    input: NativeCharacterCardExportInput,
    options: NativeFileJobOptions = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
): Promise<NativeFileJobResult> {
    return runNativeManagedExport(
        {
            operation: 'Native character card export',
            safLengthMismatchLabel: 'character card',
            handoffCleanupCommand: 'native_character_card_handoff_cleanup',
            destination: input.destination,
            prepareRequest: () => ({
                kind: 'export-character-card',
                ...(input.destination.type === 'desktopPath'
                    ? { destination: input.destination.path }
                    : {}),
                expectedRevision: input.expectedRevision,
                characterId: input.characterId,
                format: input.format,
                metadata: input.metadata,
            }),
        },
        options,
        dependencies,
    )
}

export function runNativeRisuModuleExport(
    input: NativeRisuModuleExportInput,
    options: NativeFileJobOptions = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
): Promise<NativeFileJobResult> {
    return runNativeManagedExport(
        {
            operation: 'Native RISUM export',
            safLengthMismatchLabel: 'RISUM',
            handoffCleanupCommand: 'native_risu_module_handoff_cleanup',
            destination: input.destination,
            prepareRequest: () => ({
                kind: 'export-risu-module',
                ...(input.destination.type === 'desktopPath'
                    ? { destination: input.destination.path }
                    : {}),
                expectedRevision: input.expectedRevision,
                moduleIndex: input.moduleIndex,
            }),
        },
        options,
        dependencies,
    )
}

export function runNativeLegacyLocalBackupExport(
    runtime: {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    destination: NativeLegacyLocalBackupDestination,
    options: NativeFileJobOptions = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
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

export function runNativeCompatibleLocalBackupExport(
    runtime: {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    target: NativeCompatibilityTarget,
    destination: NativeLegacyLocalBackupDestination,
    options: NativeFileJobOptions = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
): Promise<NativeFileJobResult> {
    return runNativePortableBackupExport(
        'export-compatible-local-backup',
        'native-compatible-local-backup-export',
        'Native compatible local backup export',
        'native_legacy_backup_handoff_cleanup',
        runtime,
        destination,
        options,
        dependencies,
        target,
    )
}

function runNativePortableBackupExport(
    kind: 'export-legacy-local-backup' | 'export-compatible-local-backup',
    flushReason: string,
    operation: string,
    handoffCleanupCommand: string,
    runtime: {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    destination: NativeBackupDestination,
    options: NativeFileJobOptions = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
    target?: NativeCompatibilityTarget,
): Promise<NativeFileJobResult> {
    return runNativeManagedExport(
        {
            operation,
            safLengthMismatchLabel: operation,
            handoffCleanupCommand,
            destination,
            relaySafCopyProgress: true,
            prepareRequest: async () => {
                await runtime.flushPendingData(flushReason)
                if (options.signal?.aborted) throw abortError()
                const expectedRevision = runtime.revision
                return destination.type === 'desktopPath'
                    ? {
                          kind,
                          destination: destination.path,
                          expectedRevision,
                          ...(target ? { target } : {}),
                      }
                    : {
                          kind,
                          expectedRevision,
                          ...(target ? { target } : {}),
                      }
            },
        },
        options,
        dependencies,
    )
}


export function runNativeArchiveReferenceExport(
    source: NativeFileJobSource,
    destination: NativeBackupDestination,
    options: NativeFileJobOptions = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
): Promise<NativeFileJobResult> {
    return runNativeManagedExport(
        {
            operation: 'RisuNest backup',
            safLengthMismatchLabel: 'RisuNest backup',
            handoffCleanupCommand: 'native_portable_handoff_cleanup',
            destination,
            relaySafCopyProgress: true,
            prepareRequest: () => ({
                kind: 'export-portable-backup',
                source,
                selection: { library: true, deviceSections: [] },
                ...(destination.type === 'desktopPath'
                    ? { destination: destination.path }
                    : {}),
            }),
        },
        options,
        dependencies,
    )
}

export async function runNativeArchiveExport(
    runtime: NativeBlockRestoreRuntime & {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    destination: NativeBackupDestination,
    selection: NativePortableSelection,
    options: NativeFileJobOptions = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
): Promise<NativeFileJobResult> {
    let expectedRevision = runtime.revision
    let intent:
        | import('./deviceBackup/job').PortableExportIntentStore
        | undefined
    let startedId: string | undefined
    let completed = false
    const hasDevice = selection.deviceSections.length > 0
    try {
        const result = await runNativeManagedExport(
            {
                operation: 'RisuNest backup',
                safLengthMismatchLabel: 'RisuNest backup',
                handoffCleanupCommand: 'native_portable_handoff_cleanup',
                destination,
                relaySafCopyProgress: true,
                prepareRequest: async () => {
                    if (hasDevice) {
                        const module = await import('./deviceBackup/job')
                        intent = module.createPortableExportIntentStore()
                        if (intent.read())
                            throw new NativeFileJobError(
                                'publication-pending',
                                'Finish the previous backup save before starting another',
                            )
                    }
                    await runtime.flushPendingData('native-portable-export')
                    expectedRevision = runtime.revision
                    return {
                        kind: 'export-portable-backup',
                        expectedRevision,
                        selection,
                        ...(destination.type === 'desktopPath'
                            ? { destination: destination.path }
                            : {}),
                    }
                },
            },
            {
                ...options,
                onStarted: async (jobId) => {
                    startedId = jobId
                    if (hasDevice) {
                        const module = await import('./deviceBackup/job')
                        module.rememberPortableExport(
                            jobId,
                            destination,
                            intent,
                        )
                    }
                    await options.onStarted?.(jobId)
                },
                onNativeStatus: async (status) => {
                    await options.onNativeStatus?.(status)
                },
            },
            dependencies,
        )
        completed = !result.warningCodes.includes('cleanup-failed')
        return result
    } catch (error) {
        if (startedId && intent) {
            try {
                // Only a confirmed absence clears a failed pre-restart operation. A retained
                // handoff or unknown native status remains available to publication recovery.
                const pending = (await invokeNative(
                    dependencies,
                    'native_file_job_list',
                )) as NativeFileJobStatus[]
                if (!pending.some((job) => job.jobId === startedId))
                    intent.clear(startedId)
            } catch {}
        }
        throw error
    } finally {
        if (completed && startedId) intent?.clear(startedId)
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

    const started = (await invokeNative(dependencies, 'native_file_job_start', {
        request: {
            kind: 'prepare-content-import',
            source,
            displayName,
        },
    })) as { jobId: string; warningCodes?: string[] }
    let cancellationRequested = false

    const forget = async (): Promise<void> => {
        await invokeNative(dependencies, 'native_file_job_forget', {
            jobId: started.jobId,
        })
    }
    const forgetBestEffort = async (): Promise<void> => {
        try {
            await forget()
        } catch {}
    }
    const releaseAndForget = async (
        outcome: 'committed' | 'aborted',
    ): Promise<void> => {
        let releaseFailed = false
        let releaseError: unknown
        try {
            await releaseCasJob(started.jobId, outcome, dependencies.invoke)
        } catch (error) {
            releaseFailed = true
            releaseError = error
        }
        let forgetFailed = false
        let forgetError: unknown
        try {
            await forget()
        } catch (error) {
            forgetFailed = true
            forgetError = error
        }
        if (releaseFailed) throw releaseError
        if (forgetFailed) throw forgetError
    }
    const abortAndForgetBestEffort = async (): Promise<void> => {
        try {
            await releaseAndForget('aborted')
        } catch {}
    }
    const cancelAndDrain = async (reportStatus = true): Promise<void> => {
        if (!cancellationRequested) {
            cancellationRequested = true
            await invokeNative(dependencies, 'native_file_job_cancel', {
                jobId: started.jobId,
            })
        }
        let terminal: NativeFileJobStatus
        while (true) {
            const status = (await invokeNative(
                dependencies,
                'native_file_job_status',
                {
                    jobId: started.jobId,
                },
            )) as NativeFileJobStatus
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
            const status = (await invokeNative(
                dependencies,
                'native_file_job_status',
                {
                    jobId: started.jobId,
                },
            )) as NativeFileJobStatus
            lastStatus = status
            options.onStatus?.(status)
            if (options.signal?.aborted) {
                if (status.state === 'succeeded')
                    await abortAndForgetBestEffort()
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
                    status.error?.message ??
                        'Native content preparation failed',
                )
                await forgetBestEffort()
                cleanupAttempted = true
                throw error
            }

            const content = validatePreparedContent(
                status.preparedContent,
                started.jobId,
            )
            let lifecycleState:
                | 'unfinalized'
                | 'finalizing'
                | 'finalized'
                | 'settling'
                | 'settled' = 'unfinalized'
            let finalizerOperation:
                Promise<PreparedImmutablePayload> | undefined
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
            const prepareOwnerManifestAndSeal = async (
                bytes: Uint8Array,
            ): Promise<PreparedImmutablePayload> => {
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
                } catch (error) {
                    if (!cancellationDuringFinalize) {
                        settle(() => releaseAndForget('aborted'))
                    }
                    try {
                        await settlement
                    } catch {}
                    throw error
                }
                if (cancellationDuringFinalize) {
                    try {
                        await settlement
                    } catch {}
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
                } catch (error) {
                    if (!cancellationDuringFinalize)
                        settle(() => releaseAndForget('aborted'))
                    try {
                        await settlement
                    } catch {}
                    throw error
                }
                if (cancellationDuringFinalize) {
                    try {
                        await settlement
                    } catch {}
                    throw abortError()
                }
                lifecycleState = 'finalized'
            }
            const confirmActivated = async (): Promise<void> => {
                if (lifecycleState === 'settled') return
                if (lifecycleState !== 'finalized') {
                    throw new Error(
                        'Native content activation cannot be confirmed before finalizing',
                    )
                }
                await settle(() => releaseAndForget('committed'))
            }
            const abortPreparedContent = async (): Promise<void> => {
                if (lifecycleState === 'settled') return
                if (settlement) return settlement
                if (lifecycleState === 'finalizing') {
                    cancellationDuringFinalize = true
                    await settle(async () => {
                        try {
                            await finalizerOperation
                        } catch {}
                        await releaseAndForget('aborted')
                    })
                    return
                }
                await settle(() => releaseAndForget('aborted'))
            }
            const cancel = async (): Promise<void> => {
                if (lifecycleState === 'settled') return
                if (settlement) return settlement
                if (lifecycleState === 'finalizing') {
                    cancellationDuringFinalize = true
                    await settle(async () => {
                        try {
                            await finalizerOperation
                        } catch {}
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
                warningCodes: mergeWarningCodes(
                    started.warningCodes,
                    status.warningCodes,
                ),
                prepareOwnerManifestAndSeal,
                sealPreparedContent,
                abortPreparedContent,
                confirmActivated,
                cancel,
            }
        }
    } catch (error) {
        if (!cleanupAttempted) {
            if (lastStatus?.state === 'succeeded') {
                await abortAndForgetBestEffort()
            } else if (lastStatus && isTerminalJob(lastStatus)) {
                await forgetBestEffort()
            } else {
                try {
                    await cancelAndDrain(false)
                } catch {}
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
        !isBoundedString(result.accountId, 512, false) ||
        (result.session !== null &&
            !isBoundedString(result.session, 4_096, true)) ||
        !isBoundedString(result.saveDate, 128, false) ||
        !Number.isInteger(result.status) ||
        (result.status as number) < 100 ||
        (result.status as number) > 599
    ) {
        throw new NativeFileJobError(
            'invalid-result',
            'Native official publication returned invalid publication metadata',
        )
    }
    switch (result.kind) {
        case 'written':
            if (
                isBoundedString(result.replacementKey, 4_096, false) &&
                (result.warning === null ||
                    isBoundedString(result.warning, 4_096, true)) &&
                typeof result.reloadSession === 'boolean'
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
            if (result.warning === null || isBoundedString(result.warning, 4_096, true)) {
                return result as unknown as NativeOfficialPublicationAttemptResult
            }
            break
    }
    throw new NativeFileJobError(
        'invalid-result',
        'Native official publication returned invalid publication metadata',
    )
}

function isBoundedString(
    value: unknown,
    maximumLength: number,
    allowEmpty: boolean,
): value is string {
    return (
        typeof value === 'string' &&
        value.length <= maximumLength &&
        (allowEmpty || value.length > 0)
    )
}

function areBoundedWarningCodes(value: unknown): value is string[] {
    return (
        Array.isArray(value) &&
        value.length <= 64 &&
        value.every((code) => isBoundedString(code, 128, false))
    )
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
    const publication = assertOfficialPublicationResult(
        terminal.result.publication,
    )
    if (
        terminal.jobId !== expected.jobId ||
        terminal.kind !== 'official-publication-upload' ||
        (expected.revision !== undefined &&
            terminal.result.revision !== expected.revision) ||
        (expected.accountId !== undefined &&
            publication.accountId !== expected.accountId) ||
        (expected.saveDate !== undefined &&
            publication.saveDate !== expected.saveDate) ||
        !Number.isSafeInteger(terminal.result.revision) ||
        terminal.result.revision < 0 ||
        !Number.isSafeInteger(terminal.result.sourceBytes) ||
        terminal.result.sourceBytes < 0 ||
        typeof terminal.result.sourceSha256 !== 'string' ||
        !/^[0-9a-f]{64}$/.test(terminal.result.sourceSha256) ||
        !Number.isSafeInteger(terminal.result.characterCount) ||
        terminal.result.characterCount < 0 ||
        !Number.isSafeInteger(terminal.result.presetCount) ||
        terminal.result.presetCount < 0 ||
        !areBoundedWarningCodes(startWarningCodes) ||
        !areBoundedWarningCodes(terminal.warningCodes ?? []) ||
        !areBoundedWarningCodes(terminal.result.warningCodes)
    ) {
        throw new NativeFileJobError(
            'invalid-result',
            'Native official publication returned mismatched association metadata',
        )
    }

    const result = {
        ...terminal.result,
        publication,
        warningCodes: mergeWarningCodes(
            startWarningCodes,
            terminal.warningCodes,
            terminal.result.warningCodes,
        ),
    } as NativeOfficialPublicationReceipt['result']
    let acknowledged = false
    let acknowledgement: Promise<void> | undefined

    return {
        jobId: terminal.jobId,
        result,
        acknowledge: async () => {
            if (acknowledged) return
            acknowledgement ??= invokeNative(
                dependencies,
                'native_file_job_forget',
                {
                    jobId: terminal.jobId,
                },
            )
                .then(() => {
                    acknowledged = true
                })
                .finally(() => {
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
        const value = await invokeNative(
            dependencies,
            'native_file_job_start',
            {
                request: {
                    kind: 'official-publication-upload',
                    ...request,
                },
            },
        )
        if (
            !value ||
            typeof value !== 'object' ||
            !('jobId' in value) ||
            typeof value.jobId !== 'string' ||
            value.jobId.length === 0 ||
            ('warningCodes' in value &&
                (!Array.isArray(value.warningCodes) ||
                    value.warningCodes.some(
                        (code) => typeof code !== 'string',
                    )))
        ) {
            throw new NativeFileJobError(
                'invalid-result',
                'Native official publication start returned no job ID',
            )
        }
        started = value as unknown as typeof started
    } catch (error) {
        if (
            error instanceof NativeFileJobError &&
            error.code === 'capability-unavailable'
        ) {
            return null
        }
        throw error
    }
    return await pollNativeOfficialPublication(
        started.jobId,
        {
            jobId: started.jobId,
            revision: request.expectedRevision,
            accountId: request.accountId,
            saveDate: request.saveDate,
        },
        options,
        dependencies,
        {
            startWarningCodes: started.warningCodes,
            terminalFailure: 'throw',
        },
    )
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

    await invokeNative(
        dependencies,
        'native_file_job_official_publication_retry',
        {
            request: {
                jobId,
                accountId: request.accountId,
                session: request.session,
                saveDate: request.saveDate,
                credential: request.credential,
            },
        },
    )
    const outcome = await pollNativeOfficialPublication(
        jobId,
        {
            jobId,
            revision: expected.revision,
            accountId: expected.accountId,
            saveDate: request.saveDate,
        },
        options,
        dependencies,
        { terminalFailure: 'throw' },
    )
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
    } catch {}
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
            await requestNativeOfficialPublicationCancellation(
                jobId,
                dependencies,
            )
        }
        const status = (await invokeNative(
            dependencies,
            'native_file_job_status',
            {
                jobId,
            },
        )) as NativeFileJobStatus
        options.onStatus?.(status)
        if (
            status.jobId !== jobId ||
            status.kind !== 'official-publication-upload'
        ) {
            throw new NativeFileJobError(
                'invalid-result',
                'Native official publication returned a mismatched job',
            )
        }
        if (
            status.state === 'waitingForInput' &&
            status.phase === 'awaiting-publication-retry'
        ) {
            if (behavior.cancelWhenWaiting && !cancellationRequested) {
                cancellationRequested = true
                await requestNativeOfficialPublicationCancellation(
                    jobId,
                    dependencies,
                )
            }
            if (!cancellationRequested) {
                const publication = assertOfficialPublicationResult(
                    status.publicationAttempt,
                )
                if (
                    publication.kind !== 'reauthentication-needed' ||
                    publication.status !== 403 ||
                    (expected.accountId !== undefined &&
                        publication.accountId !== expected.accountId) ||
                    (expected.saveDate !== undefined &&
                        publication.saveDate !== expected.saveDate)
                ) {
                    await requestNativeOfficialPublicationCancellation(
                        jobId,
                        dependencies,
                    )
                    throw new NativeFileJobError(
                        'invalid-result',
                        'Native official publication returned mismatched retry metadata',
                    )
                }
                return {
                    kind: 'waiting-for-reauthentication',
                    warning: publication.warning,
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
            const error =
                status.state === 'cancelled'
                    ? abortError()
                    : new NativeFileJobError(
                          status.error?.code ?? 'publication-failed',
                          status.error?.message ??
                              'Native official publication failed',
                      )
            let forgotten = false
            try {
                await invokeNative(dependencies, 'native_file_job_forget', {
                    jobId,
                })
                forgotten = true
            } catch {}
            if (behavior.terminalFailure === 'return-null') return null
            throw forgotten
                ? drainedNativeOfficialPublicationError(error)
                : error
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
