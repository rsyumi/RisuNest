import type { PortraitRenditionDescriptor } from './characterPortraitRendition'
import type { ExportExclusions } from './exportExcludedReport'
import type { OfficialAccountAssetProgress } from './sync/officialAccountAssetOutcome'
import { nativeRestoreDeadline, NativeRestoreControlTimeout, type NativeRestoreClock } from './nativeRestoreDeadline'
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
import { readCurrentRevision } from './persistentDataStore'
import { registerCommittedWorkingSetContinuation } from './committedWorkingSetContinuation'
import {
    copyNativeExportToAndroidSaf,
    discardAndroidSafSource,
    reserveAndroidSafDestination,
    withAndroidSafDestination,
    type AndroidSafDestinationReservation,
    type AndroidSafExportKind,
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
    | { type: 'androidSeekable'; token: string }
    | { type: 'iosScoped'; token: string }
    | { type: 'savedConflict'; token: string }

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
    exportExclusions?: ExportExclusions
    revision: number
    sourceBytes: number
    sourceSha256: string
    sourceFingerprintKind: 'whole-file-sha256' | 'portable-catalog-sha256'
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

export interface NativeSnapshotRestoreRequest {
    snapshotId: string
}

/** What a snapshot restore activated, which the copy of its missing bodies is bound to. */
export interface NativeSnapshotRestoreActivation {
    stagingId: string
    activationRevision: number
    bindingAuthority: string
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
export interface NativeSnapshotBodyResult {
    stageId: string
    activatedRevision: number
    bindingAuthority: string
    policy: 'full' | 'remote'
    total: number
    locallyPresent: number
    remoteHeld: number
    unavailable: number
    allBodiesLocal: boolean
    settled: boolean
}

export interface NativeSnapshotBodiesStarted {
    jobId: string
    kind: 'snapshot-bodies'
    stagingId: string
    activationRevision: string
    bindingAuthority: string
}

export type NativeFileJobStage =
    | 'downloading-account-backup'
    | 'checking-account-assets'
    | 'downloading-account-assets'
    | 'finishing-account-restore'
    | 'reading-archive'
    | 'requesting-backup-key'
    | 'decrypting-database'
    | 'preparing-attachments'
    | 'reading-database'
    | 'decoding-database'
    | 'finalizing-staging'
    | 'assign-plugin-values'
    | 'awaiting-activation'
    | 'activating'
    | 'cleaning-up'
    | 'copying-assets'
    | 'copying-source'
    | 'refreshing-app'
    | 'reloading-plugins'
    | 'restarting-app'
    | 'awaiting-reselect'
    | 'preparing-export'
    | 'downloading-export'
    | 'writing-export'
    | 'verifying-export'
    | 'publishing-destination'
    | 'finalizing-export'

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

/** Which user-facing format a dialog-presented operation belongs to. */
export type NativeFileOperationFormat =
    | 'dataset'
    | 'risu-save'
    | 'local-backup'
    | 'library-backup'
    | 'content'
    | 'raw-recovery'

/**
 * Resolves the stage a status describes. Statuses carrying `detail` name it
 * directly; older statuses (and jobs that never report detail) fall back to
 * the coarse job phase, which only knows the format-dependent first stage.
 */
export function resolveNativeFileJobStage(
    status: NativeFileJobStatus,
    format?: NativeFileOperationFormat,
): NativeFileJobStage | null {
    if (status.phase === 'copying-missing-bodies') return 'copying-assets'
    if (status.phase === 'cleaning-up') return 'cleaning-up'
    // The assignment pass happens inside the activation wait, so it is named
    // before the wait's own stage.
    if (status.phase === 'awaiting-activation' && status.pluginValuePreview?.values.length) {
        return 'assign-plugin-values'
    }
    if (status.detail) return status.detail.stage
    // A failed or cancelled job also reports `complete`; the stage it stopped in stays current.
    const endedEarly = status.state === 'failed' || status.state === 'cancelled'
    if (status.kind === 'official-publication-upload') {
        switch (status.phase) {
            case 'queued':
            case 'reading-source':
                return 'preparing-export'
            case 'writing-export':
                return 'writing-export'
            case 'uploading-database':
            case 'awaiting-publication-retry':
                return 'publishing-destination'
            case 'finalizing-publication':
                return 'finalizing-export'
            case 'complete':
                return endedEarly ? null : 'finalizing-export'
            default:
                return null
        }
    }
    // Exports report phases only. The rescue archive keeps the archive wording it always had.
    if (status.kind.startsWith('export-') && status.kind !== 'export-raw-recovery') {
        switch (status.phase) {
            case 'queued':
            case 'reading-source':
                return 'preparing-export'
            case 'writing-export':
                return 'writing-export'
            case 'publishing-destination':
                return 'publishing-destination'
            case 'finalizing-export':
                return 'finalizing-export'
            case 'complete':
                return endedEarly ? null : 'finalizing-export'
            default:
                return null
        }
    }
    switch (status.phase) {
        case 'reading-source':
            // The common picker admits every backup as a library backup, so the
            // reported job kind, not the admission format, tells an archive apart.
            return status.kind === 'restore-legacy-local-backup' ||
                format === 'local-backup' ||
                format === 'raw-recovery' ||
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

export interface NativePortableBodyRetryRequest {
    jobId: string
    stagingId: string
    catalogSha256: string
    activationRevision: string
    bindingAuthority: string
    deviceSessionId: string
}

export interface NativeFileJobStatus {
    officialAssets?: OfficialAccountAssetProgress
    officialAssetRestore?: NativeOfficialAssetRestoreProgress
    officialAssetPublication?: NativeOfficialAssetPublicationProgress
    exportCaptureRevision?: number
    portableBodyRetry?: {stagingId: string; catalogSha256: string; pending: boolean; available: boolean; sourceRequired: boolean}
    jobId: string
    kind:
        | 'restore-block-risu-save'
        | 'export-block-risu-save'
        | 'export-character-charx'
        | 'export-character-card'
        | 'export-risu-module'
        | 'export-dataset'
        | 'export-gallery-file'
        | 'export-gallery-zip'
        | 'restore-legacy-local-backup'
        | 'export-compatible-local-backup'
        | 'export-portable-backup'
        | 'export-raw-recovery'
        | 'restore-portable-backup'
        | 'snapshot-bodies'
        | 'prepare-content-import'
        | 'import-jpeg-asset'
        | 'kei-backup-upload'
        | 'restore-official-account-snapshot'
        | 'restore-native-snapshot'
        | 'official-publication-upload'
        | 'restore-official-account-assets'
        | 'publish-official-account-assets'
    expectedRevision?: number
    activationRevision?: number
    activationAuthority?: string
    restoreAdoptionConfirmed?: boolean
    snapshotStagingId?: string
    snapshotBodies?: NativeSnapshotBodyResult
    deviceSessionId?: string
    replacesLibrary?: boolean
    restorePreview?: NativePortableRestorePreview
    incompleteRestorePreview?: NativeIncompleteRestorePreview
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
        | 'copying-missing-bodies'
        | 'cleaning-up'
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
    error?: {
        code: string
        message: string
        itemKind?: string
        owner?: NativeFileJobFailureOwner
    }
}

/** The library item whose data a compatible export could not write. */
export interface NativeFileJobFailureOwner {
    kind: 'character' | 'chat' | 'preset' | 'module' | 'persona'
    name: string
    /** The character a chat belongs to. */
    character?: string
}

export interface NativePortableSelection {
    library: boolean
    deviceSections: NativePortableDeviceSection[]
    allowSourcePreservation?: boolean
}
export interface NativePortableRestorePreview {
    libraryIncluded: boolean
    repairRequired: boolean
    deviceSections: NativePortableDeviceSection[]
    /** What is wrong with the archive's library, even when the gate refuses it. */
    diagnosis?: DataHealthResult
}

export type NativeBlockRestoreRuntime = Pick<
    PersistentDataRuntime,
    | 'capturePersistentMutationToken'
    | 'acquireDestructiveReplacementFence'
    | 'markCommittedWorkingSetRefreshRequired'
    | 'getStorageAuthorityEpoch'
    | 'withPausedPersistentWrites'
    | 'beginActivatedLibraryGuard'
    | 'refreshActivatedLibraryUnderPause'
    | 'setActivatedLibraryRecoveryLifecycle'
    | 'store'
>

export interface NativeFileJobOptions {
    onStarted?(jobId: string, destinationRequestId?: string): void | Promise<void>
    onNativeStatus?(status: NativeFileJobStatus): void | Promise<void>
    signal?: AbortSignal
    pollIntervalMs?: number
    onStatus?(status: NativeFileJobStatus): void
    /** Called when an export's destination has been chosen, before the export work starts. */
    onDestinationChosen?(): void
}

export interface PreparedNativeContentActivationLifecycle {
    stageInlineAsset?(bytes: Uint8Array, name: string): Promise<PreparedCardContentAssetDescriptor>
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

export interface NativeIncompleteRestorePreview {
    unavailableColdKeys: string[]
    characterNames: string[]
    invalidInlays: string[]
    omissions?: Partial<Record<'data' | 'remote' | 'missing' | 'values' | 'pluginStorage' | 'damagedPluginStorage' | 'oversizedPluginStorage' | 'inlays' | 'excludedCharacters' | 'missingAssets', number>>
}

export interface NativeFileRestoreJobOptions extends NativeFileJobOptions {
    confirmationWarnings?: string[]
    /** The restore began as an onboarding import; an unbound device needs no overwrite acknowledgement. */
    firstRun?: boolean
    holdServerSyncAfterRestore?: boolean
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
    afterRefresh?(pluginsAlreadyRestarted?: boolean): void | Promise<void>
    onBlockingChange?(blocking: boolean): void
    afterPortableAdoption?(status: NativeFileJobStatus): Promise<void>
    /** Updates the original operation after read-only activation recovery. */
    onRecoveredStatus?(status: NativeFileJobStatus): void
    /** Runs last when the app recovers a committed restore whose refresh failed. */
    afterCommittedRecovery?(jobId: string): Promise<void>
}

export interface NativeFileExportJobOptions extends NativeFileJobOptions {
    omitAccount?: boolean
}

export interface NativeCharacterCharxExportInput {
    portraitRendition?: PortraitRenditionDescriptor
    characterId: string
    destination: NativeCharacterCharxExportDestination
    expectedRevision: number
    card: Record<string, unknown>
    module: Record<string, unknown>
    container?: 'appended-charx-jpeg'
}

export interface NativeCharacterCardExportInput {
    portraitRendition?: PortraitRenditionDescriptor
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

export interface NativeDatasetExportInput {
    destination: NativeCharacterCharxExportDestination
    expectedRevision: number
}

export type NativeCharacterCharxExportDestination =
    | { type: 'desktopPath'; path: string }
    /** A destination reserved by the caller stays the caller's to release. */
    | { type: 'androidSaf'; suggestedName: string; reservation?: AndroidSafDestinationReservation }
    | { type: 'iosFiles'; suggestedName: string }

export type NativeBackupDestination =
    | { type: 'desktopPath'; path: string }
    | { type: 'androidSaf'; suggestedName: string }
    | { type: 'iosFiles'; suggestedName: string }

export type NativeLegacyLocalBackupDestination = NativeBackupDestination

export interface NativeFileJobDependencies {
    restoreClock?: NativeRestoreClock
    isTauri(): boolean
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    wait(milliseconds: number): Promise<void>
    discardAndroidSource?(token: string): boolean | Promise<boolean>
}

export interface NativeBackupExportDependencies extends NativeFileJobDependencies {
    reserveAndroidDestination?: typeof reserveAndroidSafDestination
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
    reserveAndroidDestination: reserveAndroidSafDestination,
}

export class NativeFileJobError extends Error {
    constructor(
        readonly code: string,
        message: string,
        readonly warningCodes: string[] = [],
        readonly itemKind?: string,
        readonly owner?: NativeFileJobFailureOwner,
    ) {
        super(message)
        this.name = 'NativeFileJobError'
    }
}

const isSyncUnavailable = (error: unknown): boolean =>
    typeof error === 'object' && error !== null && 'code' in error && error.code === 'sync-unavailable'

/** The restore committed its data, but some asset bodies are still missing. */
export const PORTABLE_BODIES_INCOMPLETE = 'portable-bodies-incomplete'

export class NativeFileJobActivationCommittedError extends NativeFileJobError {
    readonly recoveryRequired = true

    constructor(
        readonly committedRevision: number,
        readonly cause: unknown,
        code = 'activation-committed-refresh-failed',
    ) {
        super(
            code,
            `Native restore committed revision ${committedRevision}, but the active app state could not be fully refreshed`,
        )
        this.name = 'NativeFileJobActivationCommittedError'
    }
}

export class NativeFileJobActivationUnresolvedError extends NativeFileJobError {
    readonly recoveryRequired = true
    constructor(readonly cause: unknown) {
        super('activation-outcome-unresolved', 'Backup activation must be checked before continuing')
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

function validatePreparedAssetDescriptors(format: unknown, values: unknown[]): PreparedContentAssetDescriptor[] {
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
    const assets = values.map(
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
            const risum = format === 'risu-module'
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
            if (mime.length === 0 && format !== 'png-card' && !risum) {
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
    return assets
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
    const assets = validatePreparedAssetDescriptors(content.format, content.assets)
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
                !/^native-inline-[0-9a-f-]{36}$/.test(asset.token) && (
                    asset.ext !== 'png' ||
                    asset.logicalId !== `assets/${asset.objectHash}.png` ||
                    asset.name !== `${asset.objectHash}.png`
                )
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
            throw new NativeFileJobError(error.code, error.message, [],
                'itemKind' in error && typeof error.itemKind === 'string' ? error.itemKind : undefined)
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

async function abortBeforeNativeRestoreStart(
    source: NativeFileJobSource | NativeOfficialAccountSnapshotRestoreRequest | NativeSnapshotRestoreRequest,
    dependencies: NativeFileJobDependencies,
): Promise<never> {
    if (!('type' in source)) throw abortError()
    let discarded = source.type !== 'androidSpool'
    if (source.type === 'androidSpool') {
        try {
            discarded =
                await dependencies.discardAndroidSource?.(source.token) === true
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

function cancellableRestorePreparation<T>(work: Promise<T>, signal?: AbortSignal, lateResult?: (value: T) => Promise<void>): Promise<T> {
    return new Promise((resolve, reject) => {
        let cancelled = false
        const abort = () => { cancelled = true; reject(abortError()) }
        signal?.addEventListener('abort', abort, { once: true })
        if (signal?.aborted) abort()
        work.then(value => {
            signal?.removeEventListener('abort', abort)
            if (cancelled) void lateResult?.(value).catch(() => {})
            else resolve(value)
        }, error => {
            signal?.removeEventListener('abort', abort)
            reject(error)
        })
    })
}

async function runNativeReplacementRestore(
    kind:
        | 'restore-block-risu-save'
        | 'restore-portable-backup'
        | 'restore-legacy-local-backup'
        | 'restore-official-account-snapshot'
        | 'restore-native-snapshot',
    mutationReason: string,
    runtime: NativeBlockRestoreRuntime,
    source: NativeFileJobSource | NativeOfficialAccountSnapshotRestoreRequest | NativeSnapshotRestoreRequest,
    options: NativeFileRestoreJobOptions & {
        choosePortableSections?(
            preview: NativePortableRestorePreview,
        ): Promise<NativePortableSelection | null>
    } = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    const originalDependencies = dependencies
    const bounded = <T>(work: Promise<T>, operation: string, lateResult?: (value: T) => void | Promise<void>): Promise<T> =>
        nativeRestoreDeadline(work, operation, originalDependencies.restoreClock, lateResult)
    dependencies = {
        ...originalDependencies,
        invoke: (command, args) => nativeRestoreDeadline(
            originalDependencies.invoke(command, args), command, originalDependencies.restoreClock,
            command === 'native_file_job_start' ? async value => {
                const id = (value as { jobId?: string })?.jobId
                if (id) await bounded(originalDependencies.invoke('native_file_job_cancel', { jobId: id }), 'late start cancellation')
            } : undefined,
        ),
    }
    const operation =
        kind === 'restore-official-account-snapshot'
            ? 'Native official account snapshot restore'
            : kind === 'restore-native-snapshot'
              ? 'Native snapshot restore'
              : kind === 'restore-legacy-local-backup'
              ? 'Native legacy local backup restore'
              : 'Native block RisuSave restore'
    if (!dependencies.isTauri()) {
        throw new Error(`${operation} requires Tauri`)
    }
    if (options.signal?.aborted) {
        await abortBeforeNativeRestoreStart(source, dependencies)
    }

    const mutationToken = await cancellableRestorePreparation(
        runtime.capturePersistentMutationToken(mutationReason, { publishOfficial: false }), options.signal,
    ).catch(async error => {
        if (options.signal?.aborted) await abortBeforeNativeRestoreStart(source, dependencies)
        throw error
    })
    if (options.signal?.aborted) {
        await abortBeforeNativeRestoreStart(source, dependencies)
    }
    const request =
        kind === 'restore-official-account-snapshot'
            ? {
                  kind,
                  ...(source as NativeOfficialAccountSnapshotRestoreRequest),
                  expectedRevision: mutationToken.revision,
              }
            : kind === 'restore-native-snapshot'
              ? {
                    kind,
                    snapshotId: (source as NativeSnapshotRestoreRequest).snapshotId,
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
        kind !== 'restore-portable-backup'
    let terminal: NativeFileJobStatus | undefined
    let observedStatus: NativeFileJobStatus | undefined
    let activationUnresolved = false
    let mutationConflict: NativeFileJobError | undefined
    let upstreamPause: Awaited<ReturnType<typeof import('./upstreamReplacement')['acquireUpstreamImportPause']>> | undefined
    let upstreamBinding: Awaited<ReturnType<typeof import('./sync/bindingRegistry')['prepareBoundLibraryReplacement']>> | undefined
    let activationSubmitted = false
    let portableAdoptedRevision: number | undefined
    let upstreamFenced = false
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
    let portableDeviceAcknowledged=false
    const confirmPortableAdoption=async(status:NativeFileJobStatus):Promise<void>=>{
        if (!status.activationAuthority || !status.deviceSessionId || !Number.isSafeInteger(status.activationRevision)) throw new NativeFileJobError('invalid-activation-receipt','Portable activation identity is unavailable')
        await invokeNative(dependencies,'native_portable_confirm_restore_adoption',{
            jobId:started.jobId,
            activationRevision:String(status.activationRevision),
            bindingAuthority:status.activationAuthority,
            deviceSessionId:status.deviceSessionId,
        })
    }
    const preparePortableDeviceReceipt=async(status:NativeFileJobStatus):Promise<void>=>{
        if (!status.deviceSessionId) throw new NativeFileJobError('invalid-activation-receipt','Portable activation has no device session')
        if (!portableDeviceAcknowledged) {
            await invokeNative(dependencies,'native_device_backup_recovery_complete',{sessionId:status.deviceSessionId})
            portableDeviceAcknowledged=true
        }
        const opened=await invokeNative(dependencies,'pds_open') as {revision?:unknown}
        if (!Number.isSafeInteger(opened?.revision)||(opened.revision as number)<status.activationRevision!) throw new NativeFileJobError('invalid-activation-receipt','Persistent store opened before portable activation')
        const {refreshDeviceStateAfterRestore}=await import('./deviceStateRestore')
        await bounded(refreshDeviceStateAfterRestore(), 'device settings refresh')
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
            observedStatus = status
            if (['succeeded', 'failed', 'cancelled'].includes(status.state))
                terminal = status
            try {
                await bounded(Promise.resolve(options.onNativeStatus?.(status)), 'status follow-up')
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
                status.state === 'running' && portableAdoptedRevision === undefined &&
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
                const requiredSections=['hypa','local-plugins','local-settings'] as const
                if (!selection.library || selection.deviceSections.length!==3 || new Set(selection.deviceSections).size!==3 || requiredSections.some(section=>!selection.deviceSections.includes(section)||!status.restorePreview!.deviceSections.includes(section)) || !status.restorePreview.libraryIncluded) {
                    throw new NativeFileJobError('invalid-full-backup-scope','A full backup requires every section')
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
                    {
                        const {prepareBoundLibraryReplacement} = await import('./sync/bindingRegistry')
                        upstreamBinding = await prepareBoundLibraryReplacement({control: bounded})
                        const {acquireUpstreamImportPause, confirmUpstreamLibraryReplacement} = await import('./upstreamReplacement')
                        const {language} = await import('../../lang')
                        const warnings: string[] = [...(options.confirmationWarnings ?? [])]
                        if (kind === 'restore-official-account-snapshot') warnings.push(language.risuNest.backup.officialRestoreInlayWarning)
                        const preview = status.incompleteRestorePreview
                        if (preview?.unavailableColdKeys.length) warnings.push(language.errors.coldStorageIncompleteRestoreConfirm(preview.characterNames.join(', '), preview.unavailableColdKeys.length, 0))
                        if (preview?.invalidInlays.length) warnings.push(language.portableBackup.invalidInlaysConfirm(preview.invalidInlays.length))
                        if (preview?.omissions) {
                            for (const key of ['data', 'remote', 'missing', 'values', 'pluginStorage', 'damagedPluginStorage', 'oversizedPluginStorage', 'inlays', 'excludedCharacters', 'missingAssets'] as const) {
                                const count = preview.omissions[key] ?? 0
                                if (count > 0) warnings.push(language.portableBackup.omittedRestoreData[key](count))
                            }
                        }
                        if (!(await confirmUpstreamLibraryReplacement(upstreamBinding.bound, warnings, options))) {
                            cancellationRequested = true
                            await invokeNative(dependencies, 'native_file_job_cancel', {jobId:started.jobId})
                            continue
                        }
                        const binding = upstreamBinding
                        try {
                            await binding.fence()
                        } catch (error) {
                            await binding.resume().catch(() => {})
                            throw error
                        }
                        upstreamFenced = true
                        const plugins = await import('../plugins/apiV3/v3.svelte')
                        const pluginPreparation = new AbortController()
                        try { await bounded(plugins.fencePluginExecutionForAuthorityReplacement(pluginPreparation.signal), 'plugin preparation') }
                        catch (error) { pluginPreparation.abort(); throw error }
                        if (kind==='restore-portable-backup') {
                            const {flushDeviceStateBeforeRestore}=await import('./deviceStateRestore')
                            await bounded(flushDeviceStateBeforeRestore(), 'device settings flush')
                        }
                        await bounded(Promise.resolve(options.beforeActivation?.()), 'preparation follow-up')
                        upstreamPause = await cancellableRestorePreparation(acquireUpstreamImportPause(runtime, `${mutationReason}-activation`), options.signal,
                            pause => bounded(pause.abortUnchanged(() => bounded(binding.assertAuthority(), 'binding verification')), 'late write pause release'))
                        replacementFence = upstreamPause.fence
                        await bounded(upstreamBinding.assertAuthority(), 'binding verification')
                    }
                } catch (error) {
                    mutationConflict = options.signal?.aborted && error instanceof Error && error.name === 'AbortError'
                        ? undefined : error instanceof NativeFileJobError
                            ? error
                            : new NativeFileJobError(
                                  error instanceof NativeRestoreControlTimeout ? error.code : isSyncUnavailable(error) ? 'sync-unavailable' : 'revision-conflict',
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
                activationSubmitted = true
                if (upstreamPause) {
                    runtime.setActivatedLibraryRecoveryLifecycle(upstreamPause.token, {
                        async beforeRefresh() {
                            await bounded(upstreamBinding!.assertAuthority(), 'binding verification')
                            if (!terminal || !['succeeded', 'failed', 'cancelled'].includes(terminal.state)) {
                                terminal = await invokeNative(dependencies, 'native_file_job_status', {jobId: started.jobId}) as NativeFileJobStatus
                                await bounded(Promise.resolve(options.onNativeStatus?.(terminal)), 'status follow-up')
                            }
                            if ((terminal.state === 'failed' || terminal.state === 'cancelled') &&
                                (kind !== 'restore-portable-backup' || !Number.isSafeInteger(terminal.activationRevision))) {
                                const current = await bounded(readCurrentRevision(runtime.store), 'activation revision check')
                                if (current !== upstreamPause!.fence.revision) throw new Error('Failed native activation changed the library')
                                return current
                            }
                            if (kind === 'restore-portable-backup') {
                                if (!Number.isSafeInteger(terminal.activationRevision)) throw new NativeFileJobError('invalid-activation-receipt','Portable activation receipt is unavailable')
                                await preparePortableDeviceReceipt(terminal)
                                return terminal.activationRevision!
                            }
                            if (!['succeeded', 'failed', 'cancelled'].includes(terminal.state)) throw new Error('Native activation is not terminal')
                            if (terminal.state === 'succeeded') {
                                if (!terminal.result) throw new Error('Native activation result is unavailable')
                                return terminal.result.revision
                            }
                            const current = await bounded(readCurrentRevision(runtime.store), 'activation revision check')
                            if (current !== upstreamPause!.fence.revision) throw new Error('Failed native activation changed the library')
                            return current
                        },
                        async afterRefresh() {
                            const plugins = await import('../plugins/apiV3/v3.svelte')
                            await bounded(plugins.invalidatePluginCachesAfterAuthorityReplacement(), 'plugin cache refresh')
                            await bounded(plugins.restartPluginsAfterAuthorityReplacement(), 'plugin restart')
                            if ((terminal?.state === 'failed' || terminal?.state === 'cancelled') &&
                                (kind !== 'restore-portable-backup' || !Number.isSafeInteger(terminal.activationRevision))) await bounded(upstreamBinding!.resume(), 'binding resume')
                            else if (kind === 'restore-portable-backup') await confirmPortableAdoption(terminal!)
                        },
                    })
                }
                try {
                    await invokeNative(dependencies, 'native_file_job_finalize', {
                        jobId: started.jobId,
                        expectedRevision: replacementFence.revision,
                    })
                } catch (error) {
                    if (!upstreamPause || !(error instanceof NativeFileJobError) || error.code !== 'library-operation-busy') throw error
                    mutationConflict = error
                    cancellationRequested = true
                    await invokeNative(dependencies, 'native_file_job_cancel', {jobId: started.jobId})
                    continue
                }
                options.onStatus?.({
                    ...status,
                    state: 'running',
                    phase: 'activating-database',
                })
            }
            if (kind === 'restore-portable-backup' && status.state==='succeeded' && status.activationRevision===undefined) {
                throw new NativeFileJobError('invalid-activation-receipt','Portable completion has no durable activation receipt')
            }
            if (kind === 'restore-portable-backup' && status.activationRevision !== undefined && portableAdoptedRevision === undefined) {
                if (!upstreamPause || !replacementFence || !Number.isSafeInteger(status.activationRevision) || status.activationRevision < replacementFence.revision) {
                    throw new NativeFileJobError('invalid-activation-receipt', 'Portable activation receipt is invalid')
                }
                terminal = status
                try {await preparePortableDeviceReceipt(status)} catch(error) {throw new NativeFileJobActivationCommittedError(status.activationRevision,error)}
                options.onStatus?.(syntheticNativeFileJobStatus(status, 'refreshing-app'))
                await replacementFence.refreshCommittedWorkingSet(status.activationRevision)
                const plugins = await import('../plugins/apiV3/v3.svelte')
                await bounded(plugins.invalidatePluginCachesAfterAuthorityReplacement(), 'plugin cache refresh')
                await bounded(plugins.restartPluginsAfterAuthorityReplacement(), 'plugin restart')
                await confirmPortableAdoption(status)
                upstreamPause.complete()
                await bounded(upstreamPause.finish(), 'write pause release')
                portableAdoptedRevision = status.activationRevision
                replacementFence = undefined
                await bounded(upstreamBinding!.resume({holdAutomaticSync: options.holdServerSyncAfterRestore}), 'binding resume')
                await bounded(Promise.resolve(options.afterPortableAdoption?.(status)), 'portable adoption follow-up')
                uiBlocking = false
                options.onBlockingChange?.(false)
                terminal = undefined
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
            if (!replacementFence && portableAdoptedRevision === undefined) {
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
            const continueAfterRefresh = async (beforeForget?: () => Promise<void>): Promise<void> => {
                try {
                    options.onStatus?.(
                        syntheticNativeFileJobStatus(terminal, 'reloading-plugins'),
                    )
                    await bounded(Promise.resolve(options.afterRefresh?.(upstreamPause ? true : undefined)), 'restore follow-up')
                } catch {
                    committedResult.warningCodes = mergeWarningCodes(
                        committedResult.warningCodes, ['post-refresh-followup-failed'],
                    )
                }
                try {
                    await bounded(Promise.resolve(beforeForget?.()), 'recovery acknowledgement')
                } finally {
                    try {
                        await invokeNative(dependencies, 'native_file_job_forget', {
                            jobId: started.jobId,
                        })
                    } catch {
                        committedResult.warningCodes = withCleanupFailedWarning(
                            committedResult.warningCodes,
                        )
                    }
                }
            }
            if (portableAdoptedRevision !== undefined) {
                if (terminal.result.revision !== portableAdoptedRevision || terminal.result.sourceFingerprintKind !== 'portable-catalog-sha256') {
                    throw new NativeFileJobError('invalid-activation-receipt','Portable completion differs from activation')
                }
                await continueAfterRefresh()
                return committedResult
            }
            try {
                options.onStatus?.(
                    syntheticNativeFileJobStatus(terminal, 'refreshing-app'),
                )
                const outcome = await replacementFence.refreshCommittedWorkingSet(terminal.result.revision)
                if (upstreamPause) {
                    const plugins = await import('../plugins/apiV3/v3.svelte')
                    await bounded(plugins.invalidatePluginCachesAfterAuthorityReplacement(), 'plugin cache refresh')
                    await bounded(plugins.restartPluginsAfterAuthorityReplacement(), 'plugin restart')
                    upstreamPause.complete()
                    await bounded(upstreamPause.finish(), 'write pause release')
                    await bounded(upstreamBinding!.resume({holdAutomaticSync: options.holdServerSyncAfterRestore}), 'binding resume')
                }
                replacementFence.release()
                replacementFence = undefined
                if (outcome.projection === 'refresh-required') {
                    registerCommittedWorkingSetContinuation(
                        terminal.result.revision,
                        runtime,
                        runtime.getStorageAuthorityEpoch(),
                        () => continueAfterRefresh(
                            async () => {
                                options.onRecoveredStatus?.(terminal!)
                                await options.afterCommittedRecovery?.(started.jobId)
                            },
                        ),
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

        if (portableAdoptedRevision !== undefined) {
            throw new NativeFileJobActivationCommittedError(portableAdoptedRevision, new NativeFileJobError(terminal.error?.code ?? 'body-completion-failed',terminal.error?.message ?? 'Portable missing bodies could not be restored'), PORTABLE_BODIES_INCOMPLETE)
        }
        const error =
            mutationConflict ??
            (terminal.state === 'cancelled'
                ? abortError()
                : new NativeFileJobError(
                      terminal.error?.code ?? 'restore-failed',
                      terminal.error?.message ?? `${operation} failed`,
                      [], terminal.error?.itemKind,
                  ))
        try {
            await invokeNative(dependencies, 'native_file_job_forget', {
                jobId: started.jobId,
            })
        } catch {}
        throw error
    } catch (error) {
        activationUnresolved = activationSubmitted && (!terminal || !['succeeded', 'failed', 'cancelled'].includes(terminal.state)
            || (error instanceof NativeFileJobError && error.code === 'activation-outcome-unresolved'))
        if (activationUnresolved) terminal = undefined
        if (portableAdoptedRevision === undefined && upstreamPause && activationSubmitted && (activationUnresolved || terminal?.state === 'succeeded')) {
            const minimumRevision = observedStatus?.activationRevision ?? terminal?.result?.revision ?? upstreamPause.fence.revision
            runtime.markCommittedWorkingSetRefreshRequired(minimumRevision, error)
            registerCommittedWorkingSetContinuation(minimumRevision, runtime, runtime.getStorageAuthorityEpoch(), async () => {
                await bounded(upstreamBinding!.assertAuthority(), 'binding verification')
                await bounded(upstreamBinding!.resume({holdAutomaticSync: terminal?.state === 'succeeded' && options.holdServerSyncAfterRestore}), 'binding resume')
                if (kind==='restore-portable-backup' && terminal) await bounded(Promise.resolve(options.afterPortableAdoption?.(terminal)), 'portable recovery follow-up')
                try { await bounded(Promise.resolve(options.afterRefresh?.(upstreamPause ? true : undefined)), 'restore follow-up') } catch {}
                if (kind === 'restore-portable-backup') {
                    const adopted = terminal!
                    terminal = await pollNativeFileJobUntilTerminal(started.jobId, {
                        onStatus: status => { if (!isTerminalJob(status)) options.onRecoveredStatus?.(status) },
                        pollIntervalMs: options.pollIntervalMs,
                    }, dependencies)
                    if (terminal.jobId !== started.jobId || terminal.activationRevision !== adopted.activationRevision
                        || terminal.activationAuthority !== adopted.activationAuthority || terminal.deviceSessionId !== adopted.deviceSessionId
                        || terminal.restoreAdoptionConfirmed !== true
                        || (terminal.state === 'succeeded' && terminal.result?.revision !== adopted.activationRevision)) {
                        throw new NativeFileJobError('invalid-activation-receipt', 'Portable recovery completion differs from activation')
                    }
                    if (terminal.portableBodyRetry?.pending) {
                        options.onRecoveredStatus?.(terminal)
                        return
                    }
                }
                if (!terminal) throw new Error('Native activation recovery is unavailable')
                options.onRecoveredStatus?.(terminal)
                if (terminal.state === 'succeeded') {
                    await bounded(Promise.resolve(options.afterCommittedRecovery?.(started.jobId)), 'recovery follow-up')
                }
                if (!terminal.portableBodyRetry?.pending) {
                    try { await invokeNative(dependencies, 'native_file_job_forget', {jobId: started.jobId}) } catch {}
                }
            }, undefined, true)
        }
        if (!terminal && !activationSubmitted) {
            try {
                await invokeNative(dependencies, 'native_file_job_cancel', {
                    jobId: started.jobId,
                })
            } catch {}
        }
        if (activationUnresolved) {
            const revision = observedStatus?.activationRevision
            if (revision !== undefined) throw new NativeFileJobActivationCommittedError(revision, error)
            throw new NativeFileJobActivationUnresolvedError(error)
        }
        throw error
    } finally {
        if (upstreamPause && portableAdoptedRevision === undefined) {
            if (!activationUnresolved && (!activationSubmitted || terminal?.state === 'cancelled' || terminal?.state === 'failed')) {
                try {
                    const unchanged = await bounded(readCurrentRevision(runtime.store), 'unchanged library check')
                    if (unchanged !== upstreamPause.fence.revision) throw new Error('Native restore changed the library')
                    const plugins = await import('../plugins/apiV3/v3.svelte')
                    await bounded(plugins.restartPluginsAfterAuthorityReplacement(), 'plugin restart')
                    // Finishing after the deadline ends the guard and the recovery that would resume the binding.
                    await bounded(upstreamPause.abortUnchanged(async () => { await bounded(Promise.resolve(upstreamBinding?.assertAuthority()), 'binding verification') }), 'unchanged restore recovery',
                        () => upstreamBinding?.resume())
                    await bounded(Promise.resolve(upstreamBinding?.resume()), 'binding resume')
                } catch { await bounded(upstreamPause.finish(), 'write pause release').catch(() => {}) }
            } else await bounded(upstreamPause.finish(), 'write pause release').catch(() => {})
        } else if (upstreamFenced && !activationSubmitted) {
            try {
                const plugins = await import('../plugins/apiV3/v3.svelte')
                await bounded(plugins.restartPluginsAfterAuthorityReplacement(), 'plugin restart')
                await bounded(upstreamBinding!.assertAuthority(), 'binding verification')
                await bounded(upstreamBinding!.resume(), 'binding resume')
            } catch {}
        }
        if (!activationSubmitted) upstreamBinding?.release()
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


export function portableBodyRetryRequest(status: NativeFileJobStatus): NativePortableBodyRetryRequest {
    const bodies = status.portableBodyRetry
    if (status.kind !== 'restore-portable-backup' || status.restoreAdoptionConfirmed !== true
        || !bodies?.pending || bodies.sourceRequired || !status.deviceSessionId
        || !Number.isSafeInteger(status.activationRevision) || !status.activationAuthority) {
        throw new NativeFileJobError('portable-body-retry-refused', 'Portable body retry is unavailable')
    }
    return {jobId: status.jobId, stagingId: bodies.stagingId, catalogSha256: bodies.catalogSha256,
        activationRevision: String(status.activationRevision), bindingAuthority: status.activationAuthority,
        deviceSessionId: status.deviceSessionId}
}

export async function retryNativePortableRestoreBodies(request: NativePortableBodyRetryRequest, options: NativeFileJobOptions = {}, dependencies: NativeFileJobDependencies = productionDependencies): Promise<NativeFileJobResult> {
    try { await invokeNative(dependencies, 'native_portable_retry_restore_bodies', {request}) }
    catch (error) { throw new NativeFileJobActivationCommittedError(Number(request.activationRevision), error, PORTABLE_BODIES_INCOMPLETE) }
    const terminal = await pollNativeFileJobUntilTerminal(request.jobId, options, dependencies)
    if (terminal.kind !== 'restore-portable-backup' || terminal.restoreAdoptionConfirmed !== true
        || String(terminal.activationRevision) !== request.activationRevision || terminal.activationAuthority !== request.bindingAuthority
        || terminal.deviceSessionId !== request.deviceSessionId || terminal.portableBodyRetry?.stagingId !== request.stagingId
        || terminal.portableBodyRetry.catalogSha256 !== request.catalogSha256) {
        throw new NativeFileJobActivationCommittedError(Number(request.activationRevision), new NativeFileJobError('portable-body-receipt-mismatch', 'Portable body retry receipt differs'), PORTABLE_BODIES_INCOMPLETE)
    }
    if (terminal.state !== 'succeeded' || !terminal.result || terminal.portableBodyRetry.pending) {
        throw new NativeFileJobActivationCommittedError(Number(request.activationRevision), new NativeFileJobError(terminal.error?.code ?? 'portable-bodies-incomplete', terminal.error?.message ?? 'Portable missing bodies could not be restored'), PORTABLE_BODIES_INCOMPLETE)
    }
    const warningCodes = mergeWarningCodes(terminal.warningCodes, terminal.result.warningCodes)
    try { await invokeNative(dependencies, 'native_file_job_forget', {jobId: request.jobId}) }
    catch { if (!warningCodes.includes('cleanup-failed')) warningCodes.push('cleanup-failed') }
    return {...terminal.result, warningCodes}
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

function snapshotRestoreActivation(status: NativeFileJobStatus | undefined): NativeSnapshotRestoreActivation | undefined {
    if (status?.kind !== 'restore-native-snapshot' || status.state !== 'succeeded' || !status.snapshotStagingId
        || !status.activationAuthority || !Number.isSafeInteger(status.activationRevision)) return undefined
    return {stagingId: status.snapshotStagingId, activationRevision: status.activationRevision!, bindingAuthority: status.activationAuthority}
}

/**
 * Restores a local snapshot. The job stages the snapshot with progress and a
 * cancel, and the library is replaced only after the replacement is confirmed.
 * `afterActivationRecovery` runs when the app later recovers a restore whose
 * refresh failed after activation.
 */
export async function runNativeSnapshotRestore(
    runtime: NativeBlockRestoreRuntime,
    request: NativeSnapshotRestoreRequest,
    options: NativeFileRestoreJobOptions & {
        afterActivationRecovery?(activation: NativeSnapshotRestoreActivation): Promise<void>
    } = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeSnapshotRestoreActivation> {
    const {afterActivationRecovery, ...restoreOptions} = options
    let activation: NativeSnapshotRestoreActivation | undefined
    const receipt = (status: NativeFileJobStatus | undefined): NativeSnapshotRestoreActivation => {
        const value = snapshotRestoreActivation(status)
        if (!value) throw new NativeFileJobError('invalid-activation-receipt', 'Snapshot activation receipt is unavailable')
        return value
    }
    const result = await runNativeReplacementRestore(
        'restore-native-snapshot',
        'native-snapshot-restore',
        runtime,
        request,
        {
            ...restoreOptions,
            onStatus(status) {
                activation = snapshotRestoreActivation(status) ?? activation
                restoreOptions.onStatus?.(status)
            },
            onRecoveredStatus(status) {
                activation = snapshotRestoreActivation(status) ?? activation
                restoreOptions.onRecoveredStatus?.(status)
            },
            afterCommittedRecovery: afterActivationRecovery && (async jobId => {
                await afterActivationRecovery(activation ?? receipt(
                    await invokeNative(dependencies, 'native_file_job_status', {jobId}) as NativeFileJobStatus,
                ))
            }),
        },
        dependencies,
    )
    if (!activation || activation.activationRevision !== result.revision) {
        throw new NativeFileJobActivationCommittedError(result.revision, new NativeFileJobError('invalid-activation-receipt', 'Snapshot activation receipt differs from its result'))
    }
    return activation
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

async function withPausedNativeExport<T>(
    runtime: Pick<PersistentDataRuntime, 'withPausedPersistentWrites'>,
    reason: string,
    options: NativeFileJobOptions,
    operation: (revision: number, options: NativeFileJobOptions) => Promise<T>,
): Promise<T> {
    const {result} = await runtime.withPausedPersistentWrites(reason, async token => {
        let ready!: () => void
        const captured = new Promise<void>(resolve => {ready = resolve})
        const result = operation(token.revision, {...options, onStatus(status) {
            if (status.exportCaptureRevision !== undefined) {
                if (status.exportCaptureRevision !== token.revision) throw new NativeFileJobError('export-capture-mismatch', 'Native export captured another revision')
                ready()
            }
            options.onStatus?.(status)
        }})
        await Promise.race([captured, result.then(() => {throw new NativeFileJobError('export-capture-missing', 'Native export capture receipt is absent')})])
        return {result}
    })
    return result
}

export function runNativeBlockRisuSaveExport(
    runtime: Pick<PersistentDataRuntime, 'withPausedPersistentWrites'>,
    destination: string,
    options: NativeFileExportJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    if (!dependencies.isTauri()) throw new Error('Native block RisuSave export requires Tauri')
    return withPausedNativeExport(runtime, 'native-block-risu-save-export', options,
        (revision, observed) => runNativeBlockRisuSaveExportAtRevision(revision, destination, {...options, ...observed}, dependencies))
}

async function runNativeBlockRisuSaveExportAtRevision(
    expectedRevision: number,
    destination: string,
    options: NativeFileExportJobOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeFileJobResult> {
    if (!dependencies.isTauri()) {
        throw new Error('Native block RisuSave export requires Tauri')
    }
    if (options.signal?.aborted) throw abortError()

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
            [], terminal.error?.itemKind,
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
    androidSourceKind?: AndroidSafExportKind
    reservation?: AndroidSafDestinationReservation
    relaySafCopyProgress?: boolean
    retainHandoffOnPublicationFailure?: boolean
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
    let reservation = spec.reservation ?? (spec.destination.type === 'androidSaf' ? spec.destination.reservation : undefined)
    let owned = false
    if (spec.destination.type === 'androidSaf' && !reservation) {
        if (!dependencies.reserveAndroidDestination) throw new NativeFileJobError('unsupported', 'Android SAF destination reservation is unavailable')
        reservation = await dependencies.reserveAndroidDestination(spec.destination.suggestedName, spec.androidSourceKind ?? 'risuSave', options.signal)
        owned = true
    }
    const run = () => runNativeManagedExportToDestination(spec, options, dependencies, reservation)
    // The destination is chosen by now, so the picker never holds the background task.
    options.onDestinationChosen?.()
    return owned ? withAndroidSafDestination(reservation, run) : run()
}

async function runNativeManagedExportToDestination(
    spec: NativeManagedExportSpec,
    options: NativeFileJobOptions,
    dependencies: NativeBackupExportDependencies,
    reservation?: AndroidSafDestinationReservation,
): Promise<NativeFileJobResult> {
    if (options.signal?.aborted) throw abortError()
    const request = await spec.prepareRequest()
    const started = (await invokeNative(dependencies, 'native_file_job_start', {
        request,
    })) as { jobId: string; warningCodes?: string[] }
    let terminal: NativeFileJobStatus
    try {
        await reservation?.bind(started.jobId)
        await options.onStarted?.(started.jobId, reservation?.requestId)
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
                [], terminal.error?.itemKind, terminal.error?.owner,
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
            if (spec.relaySafCopyProgress) options.onStatus?.({
                ...terminal, state: 'running', phase: 'publishing-destination',
                detail: {
                    stage: 'publishing-destination', stageCompleted: 0,
                    stageTotal: committedResult.sourceBytes, stageUnit: 'bytes',
                    counts: terminal.detail?.counts ?? emptyNativeImportCounts(),
                },
                progress: { completedBytes: 0, totalBytes: committedResult.sourceBytes, completedItems: 0, totalItems: 1 },
            })
            const published = await publish({
                sourcePath: managedSource,
                suggestedName: spec.destination.suggestedName,
                signal: options.signal,
                ...(reservation ? { reservation } : {}),
                ...(spec.relaySafCopyProgress
                    ? {
                          onProgress: (progress) =>
                              options.onStatus?.({
                                  ...terminal,
                                  state: 'running',
                                  phase: 'publishing-destination',
                                  detail: {
                                      stage: 'publishing-destination',
                                      stageCompleted: progress.copiedBytes,
                                      stageTotal: progress.totalBytes ?? committedResult.sourceBytes,
                                      stageUnit: 'bytes',
                                      counts: terminal.detail?.counts ?? emptyNativeImportCounts(),
                                  },
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
        const retainHandoff = Boolean(
            managedSource &&
                outcomeFailed &&
                spec.retainHandoffOnPublicationFailure,
        )
        if (managedSource && !retainHandoff) {
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
        if (!handoffCleanupFailed && !retainHandoff) {
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
                ...(input.portraitRendition ? { portraitRendition: input.portraitRendition } : {}),
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
                ...(input.portraitRendition ? { portraitRendition: input.portraitRendition } : {}),
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

export function runNativeDatasetExport(
    input: NativeDatasetExportInput,
    options: NativeFileJobOptions = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
): Promise<NativeFileJobResult> {
    return runNativeManagedExport(
        {
            operation: 'Native dataset export',
            safLengthMismatchLabel: 'dataset',
            handoffCleanupCommand: 'native_dataset_export_handoff_cleanup',
            relaySafCopyProgress: true,
            destination: input.destination,
            prepareRequest: () => ({
                kind: 'export-dataset',
                ...(input.destination.type === 'desktopPath'
                    ? { destination: input.destination.path }
                    : {}),
                expectedRevision: input.expectedRevision,
            }),
        },
        options,
        dependencies,
    )
}

export function runNativeRawRecoveryExport(
    destination: NativeBackupDestination,
    options: NativeFileJobOptions = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
): Promise<NativeFileJobResult> {
    return runNativeManagedExport(
        {
            operation: 'Original data export',
            safLengthMismatchLabel: 'original data archive',
            handoffCleanupCommand: 'native_raw_recovery_handoff_cleanup',
            destination,
            relaySafCopyProgress: true,
            retainHandoffOnPublicationFailure: true,
            prepareRequest: () => ({
                kind: 'export-raw-recovery',
                ...(destination.type === 'desktopPath'
                    ? { destination: destination.path }
                    : {}),
            }),
        },
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
    kind: 'export-compatible-local-backup',
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
            androidSourceKind: 'legacyBackup',
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


export async function runNativeArchiveExport(
    runtime: NativeBlockRestoreRuntime & {readonly revision: number; flushPendingData(reason: string): Promise<void>},
    destination: NativeBackupDestination,
    selection: NativePortableSelection,
    options: NativeFileJobOptions & { confirmSourcePreservation?(): Promise<boolean> } = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
): Promise<NativeFileJobResult> {
    if (!dependencies.isTauri()) throw new Error('RisuNest backup requires Tauri')
    if (options.signal?.aborted) throw abortError()
    const reservation = destination.type === 'androidSaf'
        ? await dependencies.reserveAndroidDestination?.(destination.suggestedName, 'risuSave', options.signal)
        : undefined
    if (destination.type === 'androidSaf' && !reservation) throw new NativeFileJobError('unsupported', 'Android SAF destination reservation is unavailable')
    options.onDestinationChosen?.()
    return withAndroidSafDestination(reservation, () =>
        withPausedNativeExport(runtime, 'native-portable-export', options,
            (revision, observed) => runNativeArchiveExportAtRevision(runtime, revision, destination, selection, {...options, ...observed}, dependencies, reservation)))
}

async function runNativeArchiveExportAtRevision(
    runtime: NativeBlockRestoreRuntime & {
        readonly revision: number
        flushPendingData(reason: string): Promise<void>
    },
    expectedRevision: number,
    destination: NativeBackupDestination,
    selection: NativePortableSelection,
    options: NativeFileJobOptions & { confirmSourcePreservation?(): Promise<boolean> } = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
    reservation?: AndroidSafDestinationReservation,
): Promise<NativeFileJobResult> {
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
                reservation,
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
                    const {flushDeviceStateBeforeRestore}=await import('./deviceStateRestore')
                    await flushDeviceStateBeforeRestore()
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
                onStarted: async (jobId, destinationRequestId) => {
                    startedId = jobId
                    if (hasDevice) {
                        const module = await import('./deviceBackup/job')
                        module.rememberPortableExport(
                            jobId,
                            destination,
                            intent,
                            destinationRequestId,
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
        if (error instanceof NativeFileJobError &&
            error.code === 'source-preservation-confirmation-required' &&
            !selection.allowSourcePreservation && options.confirmSourcePreservation) {
            if (options.signal?.aborted) throw abortError()
            const confirmed = await options.confirmSourcePreservation()
            if (options.signal?.aborted || !confirmed) throw abortError()
            return runNativeArchiveExport(runtime, destination,
                { ...selection, allowSourcePreservation: true }, options, dependencies)
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
                    [], status.error?.itemKind,
                )
                await forgetBestEffort()
                cleanupAttempted = true
                throw error
            }

            const content = validatePreparedContent(
                await invokeNative(dependencies, 'native_file_job_prepared_content', { jobId: started.jobId }),
                started.jobId,
            )
            options.signal?.throwIfAborted()
            let lifecycleState:
                | 'unfinalized'
                | 'staging'
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
            const stagedTokens = new Set(content.assets.map((asset) => asset.token))
            let stagingOperation: Promise<PreparedCardContentAssetDescriptor> | undefined
            const stageInlineAsset = (bytes: Uint8Array, name: string): Promise<PreparedCardContentAssetDescriptor> => {
                if (lifecycleState !== 'unfinalized') return Promise.reject(new Error('Native content is not awaiting asset mapping'))
                lifecycleState = 'staging'
                stagingOperation = (async () => {
                    for (let offset = 0; offset < bytes.byteLength || offset === 0;) {
                        if (lifecycleState !== 'staging' || options.signal?.aborted) throw abortError()
                        const end = Math.min(offset + 64 * 1024, bytes.byteLength)
                        const result = await dependencies.invoke(
                            'native_file_job_stage_inline_asset', {
                                jobId: started.jobId, name, totalBytes: bytes.byteLength, offset,
                                data: Array.from(bytes.subarray(offset, end)),
                            },
                        ) as { received: number, asset: unknown }
                        if (!result || result.received !== end || (end < bytes.byteLength && result.asset !== null)) {
                            throw preparedContentError('Native inline asset acknowledgement is invalid')
                        }
                        if (end === bytes.byteLength) {
                            const asset = validatePreparedAssetDescriptors('png-card', [result.asset])[0] as PreparedCardContentAssetDescriptor
                            if (asset.token !== asset.referenceKey || asset.mime !== '' || asset.byteSize !== bytes.byteLength || stagedTokens.has(asset.token)) {
                                throw preparedContentError('Native inline asset descriptor is invalid')
                            }
                            if (lifecycleState !== 'staging' || options.signal?.aborted) throw abortError()
                            stagedTokens.add(asset.token)
                            content.assets.push(asset)
                            return asset
                        }
                        offset = end
                    }
                    throw preparedContentError('Native inline asset was not completed')
                })().finally(() => {
                    if (lifecycleState === 'staging') lifecycleState = 'unfinalized'
                })
                return stagingOperation
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
                if (lifecycleState === 'staging') {
                    await settle(async () => {
                        try { await stagingOperation } catch {}
                        await releaseAndForget('aborted')
                    })
                    return
                }
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
                if (lifecycleState === 'staging') {
                    await settle(async () => {
                        try { await stagingOperation } catch {}
                        await releaseAndForget('aborted')
                    })
                    return
                }
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
                stageInlineAsset,
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

export interface NativeOfficialAssetRestoreProgress {
    installed: number
    missing: number
    failed: number
    bytes: number
    active: number
    limit: number
    waitingUntil?: number
    reason?: 'authentication' | 'reauthentication-needed' | 'local' | 'library-changed' | 'service'
}

export type NativeOfficialAssetSettled = 'installed' | 'missing' | 'failed'

export interface NativeOfficialAssetRestoreRequest {
    baseUrl: string
    credential: { kind: 'risu-auth'; token: string }
    accountId: string
    expectedLibrary: string
    keys: readonly string[]
    concurrency?: 1 | 2 | 4
    reauthenticated: boolean
}

export interface NativeOfficialAssetRestoreOptions {
    signal?: AbortSignal
    /** Runs on every poll; a failure cancels the job and is thrown once the job has stopped. */
    guard?(): Promise<void>
    settled?(key: string, result: NativeOfficialAssetSettled): void
    progress?(progress: NativeOfficialAssetRestoreProgress): void
}

export interface NativeOfficialAssetRestoreResult {
    reason?: NativeOfficialAssetRestoreProgress['reason']
    cancelled: boolean
    /** Keys the job never settled, in request order. */
    unsettled: string[]
}

const officialAssetPage = 512

export async function runNativeOfficialAssetRestore(
    request: NativeOfficialAssetRestoreRequest,
    options: NativeOfficialAssetRestoreOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeOfficialAssetRestoreResult> {
    if (!dependencies.isTauri()) throw new Error('Native official asset restore requires Tauri')
    if (options.signal?.aborted) throw abortError()
    const { keys, ...start } = request
    const started = await invokeNative(dependencies, 'native_file_job_start', {
        request: { kind: 'restore-official-account-assets', ...start, keyCount: keys.length },
    }) as { jobId?: unknown }
    if (typeof started?.jobId !== 'string' || !started.jobId) {
        throw new NativeFileJobError('invalid-result', 'Native official asset restore start returned no job ID')
    }
    const jobId = started.jobId
    const settled = new Uint8Array(keys.length)
    let read = 0
    let ended = false
    let cancelling = false
    let ownership: { error: unknown } | undefined
    const cancel = async () => {
        if (cancelling) return
        cancelling = true
        await invokeNative(dependencies, 'native_file_job_cancel', { jobId })
    }
    const readSettled = async () => {
        while (true) {
            const page = await invokeNative(dependencies, 'native_official_asset_restore_settled', {
                jobId, offset: read, limit: officialAssetPage,
            }) as { entries: { index: number; result: NativeOfficialAssetSettled }[]; total: number }
            for (const { index, result } of page.entries) {
                if (!Number.isInteger(index) || index < 0 || index >= keys.length || settled[index]
                    || (result !== 'installed' && result !== 'missing' && result !== 'failed')) {
                    throw new NativeFileJobError('invalid-result', 'Native official asset restore settled an unknown key')
                }
                settled[index] = 1
                options.settled?.(keys[index], result)
            }
            read += page.entries.length
            if (page.entries.length === 0 || read >= page.total) return
        }
    }
    const status = async () => await invokeNative(dependencies, 'native_file_job_status', { jobId }) as NativeFileJobStatus
    const poll = async () => {
        while (true) {
            if (!ownership && !cancelling) {
                try { await options.guard?.() }
                catch (error) { ownership = { error } }
            }
            if (ownership || options.signal?.aborted) await cancel()
            const current = await status()
            if (current.officialAssetRestore) options.progress?.(current.officialAssetRestore)
            await readSettled()
            if (isTerminalJob(current)) { ended = true; return current }
            await dependencies.wait(150)
        }
    }
    let terminal: NativeFileJobStatus
    try {
        for (let offset = 0; offset < keys.length && !options.signal?.aborted; offset += officialAssetPage) {
            const open = await invokeNative(dependencies, 'native_official_asset_restore_enqueue', {
                jobId, keys: keys.slice(offset, offset + officialAssetPage),
            })
            if (open !== true) break
        }
        terminal = await poll()
    } finally {
        try {
            if (!ended) {
                await cancel()
                while (!isTerminalJob(await status())) await dependencies.wait(150)
            }
            await invokeNative(dependencies, 'native_file_job_forget', { jobId })
        } catch {}
    }
    if (ownership) throw ownership.error
    if (terminal.state === 'failed') {
        throw new NativeFileJobError(terminal.error?.code ?? 'native-job-failed',
            terminal.error?.message ?? 'Native official asset restore failed', terminal.warningCodes ?? [])
    }
    return {
        reason: terminal.officialAssetRestore?.reason,
        cancelled: terminal.state === 'cancelled',
        unsettled: keys.filter((_key, index) => !settled[index]),
    }
}

export interface NativeOfficialAssetPublicationProgress {
    written: number
    failed: number
    bytes: number
    active: number
    limit: number
    waitingUntil?: number
    reason?: 'authentication' | 'reauthentication-needed' | 'local' | 'service' | 'auth-warning'
    session?: string
    warning?: string
    reloadSession: boolean
}

export interface NativeOfficialAssetPublicationRequest {
    baseUrl: string
    credential: { kind: 'risu-auth'; token: string }
    accountId: string
    /** The native lease of the revision being published, which keeps the bodies pinned. */
    lease: string
    session: string | null
    assets: readonly { localKey: string; key: string }[]
    concurrency?: 1 | 2 | 4
}

export interface NativeOfficialAssetPublicationOptions {
    signal?: AbortSignal
    /** Runs on every poll; a failure cancels the job and is thrown once the job has stopped. */
    guard?(): Promise<void>
    /** The account now holds the asset under `replacementKey`. */
    written?(key: string, replacementKey: string): void
    progress?(progress: NativeOfficialAssetPublicationProgress): void
}

export interface NativeOfficialAssetPublicationResult {
    reason?: NativeOfficialAssetPublicationProgress['reason']
    cancelled: boolean
    failed: number
    session: string | null
    warning: string | null
    reloadSession: boolean
    /** Keys the account does not hold yet, in request order. */
    unwritten: string[]
}

export async function runNativeOfficialAssetPublication(
    request: NativeOfficialAssetPublicationRequest,
    options: NativeOfficialAssetPublicationOptions = {},
    dependencies: NativeFileJobDependencies = productionDependencies,
): Promise<NativeOfficialAssetPublicationResult> {
    if (!dependencies.isTauri()) throw new Error('Native official asset publication requires Tauri')
    if (options.signal?.aborted) throw abortError()
    const { assets } = request
    const started = await invokeNative(dependencies, 'native_file_job_start', {
        request: { kind: 'publish-official-account-assets', ...request },
    }) as { jobId?: unknown }
    if (typeof started?.jobId !== 'string' || !started.jobId) {
        throw new NativeFileJobError('invalid-result', 'Native official asset publication start returned no job ID')
    }
    const jobId = started.jobId
    const written = new Uint8Array(assets.length)
    let read = 0
    let ended = false
    let cancelling = false
    let ownership: { error: unknown } | undefined
    const cancel = async () => {
        if (cancelling) return
        cancelling = true
        await invokeNative(dependencies, 'native_file_job_cancel', { jobId })
    }
    const readSettled = async () => {
        while (true) {
            const page = await invokeNative(dependencies, 'native_official_asset_publication_settled', {
                jobId, offset: read, limit: officialAssetPage,
            }) as { entries: { index: number; replacementKey: string | null }[]; total: number }
            for (const { index, replacementKey } of page.entries) {
                if (!Number.isInteger(index) || index < 0 || index >= assets.length || written[index]
                    || (replacementKey !== null && (typeof replacementKey !== 'string' || !replacementKey))) {
                    throw new NativeFileJobError('invalid-result', 'Native official asset publication settled an unknown asset')
                }
                written[index] = replacementKey === null ? 2 : 1
                if (replacementKey !== null) options.written?.(assets[index].key, replacementKey)
            }
            read += page.entries.length
            if (page.entries.length === 0 || read >= page.total) return
        }
    }
    const status = async () => await invokeNative(dependencies, 'native_file_job_status', { jobId }) as NativeFileJobStatus
    const poll = async () => {
        while (true) {
            if (!ownership && !cancelling) {
                try { await options.guard?.() }
                catch (error) { ownership = { error } }
            }
            if (ownership || options.signal?.aborted) await cancel()
            const current = await status()
            if (current.officialAssetPublication) options.progress?.(current.officialAssetPublication)
            await readSettled()
            if (isTerminalJob(current)) { ended = true; return current }
            await dependencies.wait(150)
        }
    }
    let terminal: NativeFileJobStatus
    try {
        terminal = await poll()
    } finally {
        try {
            if (!ended) {
                await cancel()
                while (!isTerminalJob(await status())) await dependencies.wait(150)
            }
            await invokeNative(dependencies, 'native_file_job_forget', { jobId })
        } catch {}
    }
    if (ownership) throw ownership.error
    if (terminal.state === 'failed') {
        throw new NativeFileJobError(terminal.error?.code ?? 'native-job-failed',
            terminal.error?.message ?? 'Native official asset publication failed', terminal.warningCodes ?? [])
    }
    const progress = terminal.officialAssetPublication
    return {
        reason: progress?.reason,
        cancelled: terminal.state === 'cancelled',
        failed: progress?.failed ?? 0,
        session: progress?.session ?? null,
        warning: progress?.warning ?? null,
        reloadSession: progress?.reloadSession ?? false,
        unwritten: assets.filter((_asset, index) => written[index] !== 1).map(asset => asset.key),
    }
}

export interface NativeGalleryExportInput {
    destination: NativeCharacterCharxExportDestination
    expectedRevision: number
    archive: boolean
    selection: {kind: 'asset'; entries: {key: string; name: string; section: string}[]}
        | {kind: 'inlay'; keys: string[]}
}

export function runNativeGalleryExport(
    input: NativeGalleryExportInput,
    options: NativeFileJobOptions = {},
    dependencies: NativeBackupExportDependencies = productionBackupExportDependencies,
): Promise<NativeFileJobResult> {
    return runNativeManagedExport({
        operation: 'Native gallery export',
        safLengthMismatchLabel: 'gallery download',
        handoffCleanupCommand: 'native_download_handoff_cleanup',
        androidSourceKind: 'download',
        destination: input.destination,
        prepareRequest: () => ({
            kind: input.archive ? 'export-gallery-zip' : 'export-gallery-file',
            ...(input.destination.type === 'desktopPath' ? {destination: input.destination.path} : {}),
            expectedRevision: input.expectedRevision,
            selection: input.selection,
        }),
    }, options, dependencies)
}
