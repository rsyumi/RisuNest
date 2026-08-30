import type { Database } from './database.svelte'
import {
    AndroidSafDestinationError,
    type AndroidSafDestinationRequest,
    type AndroidSafDestinationResult,
} from './androidSafBridge'
import type { PersistentDataRuntime } from './persistentDataRuntime.svelte'
import {
    NativeFileJobError,
    type NativeFileExportJobOptions,
    type NativeFileJobResult,
    type NativeFileJobSource,
    type NativeFileRestoreJobOptions,
} from './nativeFileJobs'
import type {
    PinnedRisuSaveExport,
    RisuSaveExportRuntime,
} from './risuSaveStoreAdapter'

type FileLike = {
    name: string
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
    platform(): 'native-desktop' | 'native-android' | 'web'
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
    reloadPlugins(): void | Promise<void>
    reloadPluginsAfterNativeRestore(): void | Promise<void>
}

function deduplicateWarningCodes(codes: string[]): string[] {
    return [...new Set(codes)].slice(0, 16)
}

async function exportThroughAndroidSaf(
    runtime: FileRouteRuntime,
    name: string,
    options: RisuSaveFileRouteOptions,
    dependencies: RisuSaveFileRouteDependencies,
): Promise<RisuSaveFileRouteResult> {
    return dependencies.withFlushedExport(
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
                    let published: AndroidSafDestinationResult
                    try {
                        published = await dependencies.copyAndroidExport({
                            sourcePath: file.path,
                            suggestedName: name,
                            signal: options.signal,
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
                    }
                    catch (error) {
                        if (
                            error
                            && typeof error === 'object'
                            && 'warningCodes' in error
                            && Array.isArray(error.warningCodes)
                        ) {
                            error.warningCodes = deduplicateWarningCodes(
                                error.warningCodes.filter((code): code is string =>
                                    typeof code === 'string'),
                            )
                        }
                        throw error
                    }
                    const warningCodes = deduplicateWarningCodes(published.warningCodes)
                    if (published.bytes !== file.bytes) {
                        throw new AndroidSafDestinationError(
                            published.requestId ?? '',
                            'byte-count-mismatch',
                            deduplicateWarningCodes([
                                ...warningCodes,
                                'partial-destination-may-remain',
                            ]),
                            `Android SAF copied ${published.bytes} of ${file.bytes} RisuSave bytes`,
                        )
                    }
                    return {
                        mode: 'native',
                        warningCodes,
                        bytes: file.bytes,
                    }
                },
            )
        },
    )
}

export interface RisuSaveFileRouteOptions extends NativeFileRestoreJobOptions {
    omitAccount?: boolean
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

async function importWithBytes(
    runtime: FileRouteRuntime,
    bytes: Uint8Array,
    dependencies: RisuSaveFileRouteDependencies,
): Promise<RisuSaveFileRouteResult> {
    const database = await dependencies.decodeRisuSave(bytes) as Database
    await runtime.replacePersistentDatabase(
        database,
        'risu-save-file-import',
        { authoritative: true },
    )
    await dependencies.reloadPlugins()
    return { mode: 'web', warningCodes: [], bytes: bytes.byteLength }
}

async function importWithWebCodec(
    runtime: FileRouteRuntime,
    dependencies: RisuSaveFileRouteDependencies,
): Promise<RisuSaveFileRouteResult | null> {
    const files = await dependencies.chooseWebImport()
    const file = files?.[0]
    if (!file) return null
    const bytes = new Uint8Array(await file.arrayBuffer())
    return importWithBytes(runtime, bytes, dependencies)
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
    if (dependencies.platform() === 'native-desktop') {
        const path = await dependencies.chooseNativeImport()
        if (!path) return null
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
        }
        catch (error) {
            if (isNativeCompatibilityFallback(error)) {
                return importWithWebCodec(runtime, dependencies)
            }
            throw error
        }
        return {
            mode: 'native',
            warningCodes: result.warningCodes,
            bytes: result.sourceBytes,
        }
    }

    return importWithWebCodec(runtime, dependencies)
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
    if (platform === 'native-desktop') {
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
        }
        catch (error) {
            if (isCapabilityUnavailable(error)) {
                return exportWithWebCodec(name, options.omitAccount ?? false, dependencies)
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
