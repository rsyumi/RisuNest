import type { Database } from './database.svelte'
import {
    AndroidSafDestinationError,
    type AndroidSafDestinationEvent,
    type AndroidSafDestinationRequest,
    type AndroidSafDestinationResult,
} from './androidSafBridge'
import type { PersistentDataRuntime } from './persistentDataRuntime.svelte'
import { listenRecoveredPublications } from './recoveredPublicationListener'
import {
    NativeFileJobError,
    syntheticNativeFileJobStatus,
    type NativeFileExportJobOptions,
    type NativeFileJobResult,
    type NativeFileJobSource,
    type NativeFileJobStage,
    type NativeFileRestoreJobOptions,
} from './nativeFileJobs'
import type { NativeFileOperationSource } from './nativeFileJobManager'
import { basenameOf } from './nativeFileSourceInfo'
import type {
    PinnedRisuSaveExport,
    RisuSaveExportRuntime,
} from './risuSaveStoreAdapter'

type FileLike = {
    name: string
    size?: number
    arrayBuffer(): Promise<ArrayBuffer>
}

type FileRouteRuntime = Pick<
    PersistentDataRuntime,
    | 'store'
    | 'revision'
    | 'flushPendingData'
    | 'capturePersistentMutationToken'
    | 'acquireDestructiveReplacementFence'
    | 'replacePersistentDatabase'
>

export interface RisuSaveFileRouteDependencies {
    platform(): 'native-desktop' | 'native-ios' | 'native-android' | 'web'
    runtime(): FileRouteRuntime
    chooseNativeImport(): Promise<string | null>
    chooseNativeExport(defaultName: string): Promise<string | null>
    chooseWebImport(): Promise<FileLike[] | null>
    runNativeImport(
        runtime: FileRouteRuntime,
        source: NativeFileJobSource,
        options: NativeFileRestoreJobOptions,
    ): Promise<NativeFileJobResult>
    runNativeExport(
        runtime: FileRouteRuntime,
        destination: string,
        options: NativeFileExportJobOptions,
    ): Promise<NativeFileJobResult>
    decodeRisuSave(bytes: Uint8Array): Promise<unknown>
    collectWebExport(omitAccount: boolean): Promise<Uint8Array>
    downloadWebExport(name: string, bytes: Uint8Array): Promise<void>
    withFlushedExport<T>(
        runtime: RisuSaveExportRuntime,
        reason: string,
        callback: (pinned: PinnedRisuSaveExport) => Promise<T>,
    ): Promise<T>
    copyAndroidExport(
        request: AndroidSafDestinationRequest,
    ): Promise<AndroidSafDestinationResult>
    markAndroidExportReady(requestId: string): boolean
    acknowledgeAndroidExport(requestId: string): boolean
    reloadPlugins(): void | Promise<void>
    reloadPluginsAfterNativeRestore(): void | Promise<void>
    /** Name and size of a picked desktop file for the progress dialog; defaults to the basename. */
    describeNativeSource?(path: string): Promise<NativeFileOperationSource>
}

function deduplicateWarningCodes(codes: string[], requiredCode?: string): string[] {
    const unique = [...new Set(codes)]
    if (!requiredCode || !unique.includes(requiredCode)) return unique.slice(0, 16)
    return [
        ...unique.filter((code) => code !== requiredCode).slice(0, 15),
        requiredCode,
    ]
}

function warningCodesFrom(error: unknown): string[] {
    if (
        !error
        || typeof error !== 'object'
        || !('warningCodes' in error)
        || !Array.isArray(error.warningCodes)
    ) return []
    const codes = error.warningCodes.filter((code): code is string => typeof code === 'string')
    return deduplicateWarningCodes(
        codes,
        codes.includes('partial-destination-may-remain')
            ? 'partial-destination-may-remain'
            : undefined,
    )
}

function requestIdFrom(error: unknown): string | null {
    if (!error || typeof error !== 'object' || !('requestId' in error)) return null
    return typeof error.requestId === 'string' && error.requestId.length > 0
        ? error.requestId
        : null
}

export function hasPartialDestinationWarning(error: unknown): boolean {
    return warningCodesFrom(error).includes('partial-destination-may-remain')
}

export function alertPartialDestinationWarning(
    error: unknown,
    warning: string,
    alert: (message: string) => void,
): boolean {
    if (!hasPartialDestinationWarning(error)) return false
    alert(warning)
    return true
}

async function exportThroughAndroidSaf(
    runtime: FileRouteRuntime,
    name: string,
    options: RisuSaveFileRouteOptions,
    dependencies: RisuSaveFileRouteDependencies,
): Promise<RisuSaveFileRouteResult> {
    const terminal = await dependencies.withFlushedExport(
        runtime,
        'risu-save-file-export',
        async (pinned) => {
            if (!pinned.withNativeFile) {
                throw new NativeFileJobError(
                    'capability-unavailable',
                    'Pinned RisuSave revision does not support a managed native export file',
                )
            }
            return pinned.withNativeFile(
                { omitAccount: options.omitAccount ?? false },
                async (file) => {
                    try {
                        const published = await dependencies.copyAndroidExport({
                            sourcePath: file.path,
                            suggestedName: name,
                            signal: options.signal,
                            deferAcknowledgement: true,
                            onProgress: (progress) => options.onStatus?.({
                                jobId: progress.requestId,
                                kind: 'export-block-risu-save',
                                state: 'running',
                                phase: 'writing-export',
                                progress: {
                                    completedBytes: progress.copiedBytes,
                                    ...(progress.totalBytes === null
                                        ? {}
                                        : { totalBytes: progress.totalBytes }),
                                    completedItems: 0,
                                    totalItems: 1,
                                },
                            }),
                        })
                        if (!published.requestId) {
                            throw new NativeFileJobError(
                                'invalid-result',
                                'Android SAF terminal receipt omitted its request ID',
                            )
                        }
                        const warningCodes = deduplicateWarningCodes(published.warningCodes)
                        if (published.bytes !== file.bytes) {
                            return {
                                requestId: published.requestId,
                                error: new AndroidSafDestinationError(
                                    published.requestId,
                                    'byte-count-mismatch',
                                    deduplicateWarningCodes([
                                        ...warningCodes,
                                        'partial-destination-may-remain',
                                    ], 'partial-destination-may-remain'),
                                    `Android SAF copied ${published.bytes} of ${file.bytes} RisuSave bytes`,
                                ),
                            }
                        }
                        return {
                            requestId: published.requestId,
                            result: {
                                mode: 'native' as const,
                                warningCodes,
                                bytes: file.bytes,
                            },
                        }
                    }
                    catch (error) {
                        const requestId = requestIdFrom(error)
                        if (!requestId) throw error
                        const warningCodes = warningCodesFrom(error)
                        if (error && typeof error === 'object' && 'warningCodes' in error) {
                            error.warningCodes = warningCodes
                        }
                        return { requestId, error }
                    }
                },
            )
        },
    )
    if (!dependencies.markAndroidExportReady(terminal.requestId)) {
        throw new AndroidSafDestinationError(
            terminal.requestId,
            'prerequisite-proof-failed',
            'error' in terminal ? warningCodesFrom(terminal.error) : terminal.result.warningCodes,
            'Android SAF publication prerequisites could not be persisted',
        )
    }
    if (!dependencies.acknowledgeAndroidExport(terminal.requestId)) {
        throw new AndroidSafDestinationError(
            terminal.requestId,
            'acknowledgement-failed',
            'error' in terminal ? warningCodesFrom(terminal.error) : terminal.result.warningCodes,
            'Android SAF terminal receipt could not be acknowledged',
        )
    }
    if ('error' in terminal) throw terminal.error
    return terminal.result
}

const UUID_V4 = /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/

function recoveredAndroidRisuSaveTerminal(
    encoded: string | null,
): AndroidSafDestinationEvent | null {
    if (!encoded || encoded.length > 8_192) return null
    let value: unknown
    try {
        value = JSON.parse(encoded)
    }
    catch {
        return null
    }
    if (!value || typeof value !== 'object') return null
    const event = value as Partial<AndroidSafDestinationEvent>
    if (
        typeof event.requestId !== 'string'
        || !UUID_V4.test(event.requestId)
        || typeof event.exportId !== 'string'
        || !UUID_V4.test(event.exportId)
        || event.sourceKind !== 'risuSave'
        || !['succeeded', 'failed', 'cancelled'].includes(event.state ?? '')
        || event.publicationPrerequisitesComplete !== true
        || !Array.isArray(event.warningCodes)
        || event.warningCodes.some((warning) => typeof warning !== 'string')
    ) return null
    return event as AndroidSafDestinationEvent
}

export function recoverAndroidRisuSavePublication(
    encoded: string | null,
    acknowledge: (requestId: string) => boolean,
): AndroidSafDestinationEvent | null {
    const terminal = recoveredAndroidRisuSaveTerminal(encoded)
    if (!terminal) return null
    if (!acknowledge(terminal.requestId)) {
        throw new AndroidSafDestinationError(
            terminal.requestId,
            'acknowledgement-failed',
            warningCodesFrom(terminal),
            'Recovered Android RisuSave terminal receipt could not be acknowledged',
        )
    }
    return terminal
}

export interface AndroidRisuSaveRecoveryDependencies {
    getStatus(): string | null
    acknowledge(requestId: string): boolean
    listen(listener: (event: AndroidSafDestinationEvent) => void): () => void
    isActive(requestId: string): boolean
}

export function listenRecoveredAndroidRisuSavePublications(
    onTerminal: (terminal: AndroidSafDestinationEvent) => void,
    onError: (error: unknown) => void,
    dependencies: AndroidRisuSaveRecoveryDependencies,
): () => void {
    const encoded = dependencies.getStatus()
    const initialTerminal = recoveredAndroidRisuSaveTerminal(encoded)
    return listenRecoveredPublications({
        sourceKind: 'risuSave',
        onTerminal,
        onError,
        listen: dependencies.listen,
        isActive: dependencies.isActive,
        recoverEvent: (event) => recoverAndroidRisuSavePublication(
            JSON.stringify(event),
            dependencies.acknowledge,
        ),
        initial: encoded && initialTerminal && !dependencies.isActive(initialTerminal.requestId)
            ? {
                requestId: initialTerminal.requestId,
                recover: () => recoverAndroidRisuSavePublication(
                    encoded,
                    dependencies.acknowledge,
                ),
            }
            : null,
    })
}

export interface RisuSaveFileRouteOptions extends NativeFileRestoreJobOptions {
    omitAccount?: boolean
    /** Receives the picked file's name and size for the progress dialog. */
    onSource?(source: NativeFileOperationSource): void
}

export interface RisuSaveFileRouteResult {
    mode: 'native' | 'web'
    warningCodes: string[]
    bytes?: number
}

function defaultExportName(): string {
    return `risunest-${new Date().toISOString().replace(/[:.]/g, '-')}.risudat`
}

function isCapabilityUnavailable(error: unknown): boolean {
    return error instanceof NativeFileJobError && error.code === 'capability-unavailable'
}

function isNativeCompatibilityFallback(error: unknown): boolean {
    return isCapabilityUnavailable(error)
        || error instanceof NativeFileJobError && error.code === 'unsupported-format'
}

const WEB_IMPORT_KIND = 'restore-block-risu-save' as const

function webImportStatus(
    options: RisuSaveFileRouteOptions,
    stage: NativeFileJobStage,
    bytes: number | undefined,
    completed: number,
): void {
    options.onStatus?.(syntheticNativeFileJobStatus({ kind: WEB_IMPORT_KIND }, stage, {
        stageUnit: 'bytes',
        stageCompleted: completed,
        ...(bytes === undefined ? {} : { stageTotal: bytes }),
        progress: {
            completedBytes: completed,
            ...(bytes === undefined ? {} : { totalBytes: bytes }),
            completedItems: 0,
        },
    }))
}

async function importWithBytes(
    runtime: FileRouteRuntime,
    bytes: Uint8Array,
    options: RisuSaveFileRouteOptions,
    dependencies: RisuSaveFileRouteDependencies,
): Promise<RisuSaveFileRouteResult> {
    webImportStatus(options, 'decoding-database', bytes.byteLength, bytes.byteLength)
    const database = await dependencies.decodeRisuSave(bytes) as Database
    webImportStatus(options, 'activating', bytes.byteLength, bytes.byteLength)
    await runtime.replacePersistentDatabase(
        database,
        'risu-save-file-import',
        { authoritative: true },
    )
    webImportStatus(options, 'reloading-plugins', bytes.byteLength, bytes.byteLength)
    await dependencies.reloadPlugins()
    return { mode: 'web', warningCodes: [], bytes: bytes.byteLength }
}

async function importWithWebCodec(
    runtime: FileRouteRuntime,
    options: RisuSaveFileRouteOptions,
    dependencies: RisuSaveFileRouteDependencies,
): Promise<RisuSaveFileRouteResult | null> {
    const files = await dependencies.chooseWebImport()
    const file = files?.[0]
    if (!file) return null
    options.onSource?.({ name: file.name, ...(file.size === undefined ? {} : { bytes: file.size }) })
    webImportStatus(options, 'reading-database', file.size, 0)
    const bytes = new Uint8Array(await file.arrayBuffer())
    return importWithBytes(runtime, bytes, options, dependencies)
}

/** Tells the dialog the native job gave up and the user must pick the file again in the WebView. */
function announceWebReselect(options: RisuSaveFileRouteOptions): void {
    options.onStatus?.(syntheticNativeFileJobStatus({ kind: WEB_IMPORT_KIND }, 'awaiting-reselect'))
}

async function exportWithWebCodec(
    name: string,
    omitAccount: boolean,
    dependencies: RisuSaveFileRouteDependencies,
): Promise<RisuSaveFileRouteResult> {
    const bytes = await dependencies.collectWebExport(omitAccount)
    await dependencies.downloadWebExport(name, bytes)
    return { mode: 'web', warningCodes: [], bytes: bytes.byteLength }
}

export async function importRisuSaveFromPicker(
    options: RisuSaveFileRouteOptions,
    dependencies: RisuSaveFileRouteDependencies,
): Promise<RisuSaveFileRouteResult | null> {
    const runtime = dependencies.runtime()
    if (['native-desktop', 'native-ios'].includes(dependencies.platform())) {
        const path = await dependencies.chooseNativeImport()
        if (!path) return null
        if (options.onSource) {
            options.onSource(
                dependencies.describeNativeSource
                    ? await dependencies.describeNativeSource(path)
                    : { name: basenameOf(path) },
            )
        }
        let result: NativeFileJobResult
        try {
            result = await dependencies.runNativeImport(
                runtime,
                { type: 'desktopPath', path },
                {
                    signal: options.signal,
                    pollIntervalMs: options.pollIntervalMs,
                    onStatus: options.onStatus,
                    onBlockingChange: options.onBlockingChange,
                    afterRefresh: dependencies.reloadPluginsAfterNativeRestore,
                },
            )
        } catch (error) {
            if (isNativeCompatibilityFallback(error)) {
                announceWebReselect(options)
                return importWithWebCodec(runtime, options, dependencies)
            }
            throw error
        }
        return {
            mode: 'native',
            warningCodes: result.warningCodes,
            bytes: result.sourceBytes,
        }
    }

    return importWithWebCodec(runtime, options, dependencies)
}

export async function exportRisuSaveFromPicker(
    options: RisuSaveFileRouteOptions,
    dependencies: RisuSaveFileRouteDependencies,
): Promise<RisuSaveFileRouteResult | null> {
    const name = defaultExportName()
    const platform = dependencies.platform()
    if (platform === 'native-android') {
        return exportThroughAndroidSaf(dependencies.runtime(), name, options, dependencies)
    }
    if (platform === 'native-desktop' || platform === 'native-ios') {
        const destination = await dependencies.chooseNativeExport(name)
        if (!destination) return null
        let result: NativeFileJobResult
        try {
            result = await dependencies.runNativeExport(
                dependencies.runtime(),
                destination,
                {
                    signal: options.signal,
                    pollIntervalMs: options.pollIntervalMs,
                    onStatus: options.onStatus,
                    omitAccount: options.omitAccount ?? false,
                },
            )
        } catch (error) {
            if (isCapabilityUnavailable(error)) {
                return exportWithWebCodec(
                    name,
                    options.omitAccount ?? false,
                    dependencies,
                )
            }
            throw error
        }
        return {
            mode: 'native',
            warningCodes: result.warningCodes,
            bytes: result.sourceBytes,
        }
    }

    return exportWithWebCodec(name, options.omitAccount ?? false, dependencies)
}
