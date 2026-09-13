import type { NativeFileJobSource } from './nativeFileJobs'

const SPOOL_EVENT = 'risu-android-spool-ready'
const BACKUP_SOURCE_EVENT = 'risu-android-backup-source-picked'
const LEGACY_BACKUP_SOURCE_EVENT = 'risu-android-legacy-backup-source-picked'
const DESTINATION_EVENT = 'risu-android-saf-destination'
const PROGRESS_EVENT = 'risu-android-saf-progress'
const activeDestinationRequestIds = new Set<string>()

export interface AndroidSpoolReady {
    token: string
    displayName: string
    bytes: number
    totalBytes?: number | null
}

export interface AndroidSpoolFailure {
    displayName: string
    code: string
}

export interface AndroidSpoolBatch {
    requestId: string
    ready: AndroidSpoolReady[]
    failures: AndroidSpoolFailure[]
}

export interface AndroidSpoolListenerDependencies {
    takePendingBatch(): AndroidSpoolBatch | null | undefined
    clearPendingBatch(): void
    addEventListener(name: string, listener: (event: Event) => void): void
    removeEventListener(name: string, listener: (event: Event) => void): void
}

const productionSpoolListenerDependencies: AndroidSpoolListenerDependencies = {
    takePendingBatch: () => {
        const target = window as Window & { tauriOpenedFileSpools?: AndroidSpoolBatch }
        const batch = target.tauriOpenedFileSpools
        delete target.tauriOpenedFileSpools
        return batch
    },
    clearPendingBatch: () => {
        delete (window as Window & { tauriOpenedFileSpools?: AndroidSpoolBatch })
            .tauriOpenedFileSpools
    },
    addEventListener: (name, listener) => window.addEventListener(name, listener),
    removeEventListener: (name, listener) => window.removeEventListener(name, listener),
}

export function listenAndroidSpoolBatches(
    listener: (batch: AndroidSpoolBatch) => void,
    dependencies: AndroidSpoolListenerDependencies = productionSpoolListenerDependencies,
): () => void {
    const onReady = (event: Event) => {
        const batch = (event as CustomEvent<AndroidSpoolBatch>).detail
        if (batch) {
            dependencies.clearPendingBatch()
            listener(batch)
        }
    }
    dependencies.addEventListener(SPOOL_EVENT, onReady)
    const initial = dependencies.takePendingBatch()
    if (initial) queueMicrotask(() => listener(initial))
    return () => dependencies.removeEventListener(SPOOL_EVENT, onReady)
}

export interface AndroidSpoolConsumer {
    restore(input: { source: NativeFileJobSource; displayName: string }): Promise<void>
    unsupported(source: AndroidSpoolReady): void
    failed?(failure: AndroidSpoolFailure): void
}

// Content-spool suffix subset. Together with the database restore suffixes
// checked in consumeAndroidSpoolBatch below, it forms the Kotlin
// NATIVE_FILE_JOB_SPOOL_SUFFIXES allowlist; the alignment is pinned by the
// shared taxonomy golden fixture (tests/fixtures/nativeFileTaxonomyV1Golden.json).
export function isAndroidNativeContentSpool(source: AndroidSpoolReady): boolean {
    const displayName = source.displayName.toLocaleLowerCase('en-US')
    return displayName.endsWith('.json')
        || displayName.endsWith('.charx')
        || displayName.endsWith('.jpg')
        || displayName.endsWith('.jpeg')
        || displayName.endsWith('.png')
        || displayName.endsWith('.risum')
}

export async function consumeAndroidSpoolBatch(
    batch: AndroidSpoolBatch,
    consumer: AndroidSpoolConsumer,
): Promise<void> {
    for (const failure of batch.failures) consumer.failed?.(failure)
    for (const source of batch.ready) {
        const lowerName = source.displayName.toLocaleLowerCase('en-US')
        if (!['.risunest', '.risudat', '.bin'].some((extension) => lowerName.endsWith(extension))) {
            consumer.unsupported(source)
            continue
        }
        await consumer.restore({
            source: { type: 'androidSpool', token: source.token },
            displayName: source.displayName,
        })
    }
}

export interface AndroidSafDestinationRequest {
    sourcePath: string
    suggestedName: string
    signal?: AbortSignal
    onProgress?(progress: AndroidSafProgress): void
    deferAcknowledgement?: boolean
}

export interface AndroidSafProgress {
    requestId: string
    operation: 'source-copy' | 'destination-copy'
    copiedBytes: number
    totalBytes: number | null
    token: string | null
}

export interface AndroidSafDestinationResult {
    requestId?: string
    bytes: number
    warningCodes: string[]
}

export interface AndroidSafDestinationEvent {
    requestId: string
    exportId?: string
    sourceKind?: 'risuSave' | 'legacyBackup' | 'screenshot'
    state: 'succeeded' | 'failed' | 'cancelled'
    bytes?: number | null
    code?: string | null
    message?: string | null
    warningCodes: string[]
    publicationPrerequisitesComplete?: boolean
}

export interface AndroidSafJavascriptBridge {
    copyExport(
        requestId: string,
        sourcePath: string,
        suggestedName: string,
    ): void
    cancelExport?(requestId: string): boolean | void
    cancelSource?(requestId: string): void
    pickBackupSource?(requestId: string): void
    pickContentSource?(requestId: string): void
    pickLegacyBackupSource?(requestId: string): void
    discardSource?(token: string): boolean
    getActiveSourceRequestIds?(): string
    getExportStatus?(): string | null
    getExportSourceId?(): string | null
    markExportPublicationReady?(requestId: string): boolean
    acknowledgeExport?(requestId: string): boolean
}

export interface AndroidSafSourcePickerOptions {
    signal?: AbortSignal
    onProgress?(progress: AndroidSafProgress): void
    /** Reports the picked file's name and size before the spool token is returned. */
    onSource?(source: { displayName: string; bytes: number }): void
}

export interface AndroidSafSourcePickerDependencies {
    createRequestId(): string
    bridge: AndroidSafJavascriptBridge
    addEventListener(name: string, listener: (event: Event) => void): void
    removeEventListener(name: string, listener: (event: Event) => void): void
}

export interface AndroidSafDestinationDependencies {
    createRequestId(): string
    bridge: AndroidSafJavascriptBridge
    addEventListener(name: string, listener: (event: Event) => void): void
    removeEventListener(name: string, listener: (event: Event) => void): void
}

export function isAndroidSafFileJobsEnabled(
    bridge: unknown = (window as Window & {
        RisuSafBridge?: AndroidSafJavascriptBridge
    }).RisuSafBridge,
): boolean {
    return !!bridge
}

function productionBridge(): AndroidSafJavascriptBridge {
    const bridge = (window as Window & {
        RisuSafBridge?: AndroidSafJavascriptBridge
    }).RisuSafBridge
    if (!bridge) throw new Error('Android SAF bridge is unavailable')
    return bridge
}

const productionDependencies: AndroidSafDestinationDependencies = {
    createRequestId: () => crypto.randomUUID(),
    get bridge() {
        return productionBridge()
    },
    addEventListener: (name, listener) => window.addEventListener(name, listener),
    removeEventListener: (name, listener) => window.removeEventListener(name, listener),
}

export class AndroidSafDestinationError extends Error {
    constructor(
        readonly requestId: string,
        readonly code: string,
        readonly warningCodes: string[],
        message: string,
    ) {
        super(message)
        this.name = 'AndroidSafDestinationError'
    }
}

export class AndroidSafSourceError extends Error {
    constructor(readonly code: string, message: string) {
        super(message)
        this.name = 'AndroidSafSourceError'
    }
}

interface AndroidSafSourcePickerConfig {
    eventName: string
    pickMethod:
        | 'pickBackupSource'
        | 'pickLegacyBackupSource'
        | 'pickContentSource'
    pickerUnavailableMessage: string
    acceptExtension: string | string[]
    cancelMessage: string
}

function pickAndroidSpoolSource(
    config: AndroidSafSourcePickerConfig,
    options: AndroidSafSourcePickerOptions,
    dependencies: AndroidSafSourcePickerDependencies,
): Promise<NativeFileJobSource | null> {
    if (options.signal?.aborted) {
        return Promise.reject(
            new DOMException(config.cancelMessage, 'AbortError'),
        )
    }
    const requestId = dependencies.createRequestId()
    return new Promise((resolve, reject) => {
        let settled = false
        let aborted = false
        const cleanup = () => {
            options.signal?.removeEventListener('abort', onAbort)
            dependencies.removeEventListener(config.eventName, onEvent)
            if (options.onProgress) {
                dependencies.removeEventListener(PROGRESS_EVENT, onProgress)
            }
        }
        const finish = (callback: () => void) => {
            if (settled) return
            settled = true
            cleanup()
            callback()
        }
        const onAbort = () => {
            if (settled) return
            aborted = true
            options.signal?.removeEventListener('abort', onAbort)
            if (options.onProgress) {
                dependencies.removeEventListener(PROGRESS_EVENT, onProgress)
            }
            dependencies.bridge.cancelSource?.(requestId)
        }
        const onEvent = (event: Event) => {
            const batch = (event as CustomEvent<AndroidSpoolBatch>).detail
            if (!batch || batch.requestId !== requestId) return
            if (aborted) {
                let cleanupFailed = false
                for (const source of batch.ready) {
                    if (
                        dependencies.bridge.discardSource?.(source.token) !==
                        true
                    ) {
                        cleanupFailed = true
                    }
                }
                finish(() =>
                    cleanupFailed
                        ? reject(
                              new AndroidSafSourceError(
                                  'cleanup-failed',
                                  'Cancelled Android backup source could not be cleaned up',
                              ),
                          )
                        : reject(
                              new DOMException(
                                  config.cancelMessage,
                                  'AbortError',
                              ),
                          ),
                )
                return
            }
            const failure = batch.failures[0]
            if (failure) {
                finish(() =>
                    reject(
                        new AndroidSafSourceError(
                            failure.code,
                            `${failure.displayName}: ${failure.code}`,
                        ),
                    ),
                )
                return
            }
            const source = batch.ready[0]
            if (!source) {
                finish(() => resolve(null))
                return
            }
            if (
                !(
                    Array.isArray(config.acceptExtension)
                        ? config.acceptExtension
                        : [config.acceptExtension]
                ).some((ext) =>
                    source.displayName.toLocaleLowerCase('en-US').endsWith(ext),
                )
            ) {
                dependencies.bridge.discardSource?.(source.token)
                finish(() =>
                    reject(
                        new AndroidSafSourceError(
                            'unsupported-format',
                            `${source.displayName}: unsupported-format`,
                        ),
                    ),
                )
                return
            }
            options.onSource?.({
                displayName: source.displayName,
                bytes: source.bytes,
            })
            finish(() => resolve({ type: 'androidSpool', token: source.token }))
        }
        const onProgress = (event: Event) => {
            const progress = (event as CustomEvent<AndroidSafProgress>).detail
            if (
                progress?.requestId === requestId &&
                progress.operation === 'source-copy'
            ) {
                options.onProgress?.(progress)
            }
        }
        dependencies.addEventListener(config.eventName, onEvent)
        if (options.onProgress) {
            dependencies.addEventListener(PROGRESS_EVENT, onProgress)
        }
        options.signal?.addEventListener('abort', onAbort, { once: true })
        try {
            const pick = dependencies.bridge[config.pickMethod]
            if (!pick) throw new Error(config.pickerUnavailableMessage)
            pick.call(dependencies.bridge, requestId)
        } catch (error) {
            finish(() => reject(error))
        }
    })
}


/** The native picker name is retained as an Android bridge command, not a file-format fallback. */
export function pickAndroidBackupSource(
    options: AndroidSafSourcePickerOptions = {},
    dependencies: AndroidSafSourcePickerDependencies = productionDependencies,
): Promise<NativeFileJobSource | null> {
    return pickAndroidSpoolSource(
        {
            eventName: BACKUP_SOURCE_EVENT,
            pickMethod: 'pickBackupSource',
            pickerUnavailableMessage: 'Android backup picker is unavailable',
            acceptExtension: ['.risunest', '.risudat', '.bin'],
            cancelMessage: 'Android backup selection was cancelled',
        },
        options,
        dependencies,
    )
}

export function pickAndroidLegacyBackupSource(
    options: AndroidSafSourcePickerOptions = {},
    dependencies: AndroidSafSourcePickerDependencies = productionDependencies,
): Promise<NativeFileJobSource | null> {
    return pickAndroidSpoolSource({
        eventName: LEGACY_BACKUP_SOURCE_EVENT,
        pickMethod: 'pickLegacyBackupSource',
        pickerUnavailableMessage: 'Android legacy backup picker is unavailable',
        acceptExtension: '.bin',
        cancelMessage: 'Android legacy backup selection was cancelled',
    }, options, dependencies)
}

function androidSafAbortError(
    requestId: string,
    detail?: Pick<AndroidSafDestinationEvent, 'code' | 'message' | 'warningCodes'>,
): DOMException & {
    requestId: string
    code?: string | null
    warningCodes: string[]
} {
    return Object.assign(
        new DOMException(
            detail?.message ?? 'Android SAF export was cancelled',
            'AbortError',
        ),
        {
            requestId,
            code: detail?.code,
            warningCodes: detail?.warningCodes ?? [],
        },
    )
}

export function isAndroidSafDestinationRequestActive(requestId: string): boolean {
    return activeDestinationRequestIds.has(requestId)
}

export function listenAndroidSafDestinationEvents(
    listener: (event: AndroidSafDestinationEvent) => void,
    dependencies: Pick<AndroidSafDestinationDependencies, 'addEventListener' | 'removeEventListener'> = productionDependencies,
): () => void {
    const onDestination = (event: Event) => {
        const detail = (event as CustomEvent<AndroidSafDestinationEvent>).detail
        if (detail) listener(detail)
    }
    dependencies.addEventListener(DESTINATION_EVENT, onDestination)
    return () => dependencies.removeEventListener(DESTINATION_EVENT, onDestination)
}

export function acknowledgeAndroidSafExport(
    requestId: string,
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): boolean {
    return bridge.acknowledgeExport?.(requestId) === true
}

export function markAndroidSafExportPublicationReady(
    requestId: string,
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): boolean {
    return bridge.markExportPublicationReady?.(requestId) === true
}

export function getAndroidSafExportStatus(
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): string | null {
    return bridge.getExportStatus?.() ?? null
}

export function getAndroidSafExportSourceId(
    bridge: AndroidSafJavascriptBridge | undefined = (window as Window & {
        RisuSafBridge?: AndroidSafJavascriptBridge
    }).RisuSafBridge,
): string | null {
    if (bridge?.getExportStatus?.() != null) return null
    const exportId = bridge?.getExportSourceId?.()
    return typeof exportId === 'string'
        && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(exportId)
        ? exportId
        : null
}

export function discardAndroidSafSource(
    token: string,
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): boolean {
    return bridge.discardSource?.(token) === true
}

export function copyNativeExportToAndroidSaf(
    request: AndroidSafDestinationRequest,
    dependencies: AndroidSafDestinationDependencies = productionDependencies,
): Promise<AndroidSafDestinationResult> {
    if (request.signal?.aborted) {
        return Promise.reject(new DOMException('Android SAF export was cancelled', 'AbortError'))
    }
    const requestId = dependencies.createRequestId()
    activeDestinationRequestIds.add(requestId)
    return new Promise((resolve, reject) => {
        let settled = false
        const cleanup = () => {
            request.signal?.removeEventListener('abort', onAbort)
            dependencies.removeEventListener(DESTINATION_EVENT, onEvent)
            queueMicrotask(() => activeDestinationRequestIds.delete(requestId))
            if (request.onProgress) {
                dependencies.removeEventListener(PROGRESS_EVENT, onProgress)
            }
        }
        const finish = (callback: () => void) => {
            if (settled) return
            settled = true
            cleanup()
            callback()
        }
        const onAbort = () => {
            dependencies.bridge.cancelExport?.(requestId)
        }
        const onEvent = (event: Event) => {
            const detail = (event as CustomEvent<AndroidSafDestinationEvent>).detail
            if (!detail || detail.requestId !== requestId) return
            if (!request.deferAcknowledgement) {
                dependencies.bridge.acknowledgeExport?.(requestId)
            }
            if (detail.state === 'succeeded' && typeof detail.bytes === 'number') {
                finish(() => resolve({
                    requestId,
                    bytes: detail.bytes as number,
                    warningCodes: detail.warningCodes,
                }))
                return
            }
            if (detail.state === 'cancelled') {
                finish(() => reject(androidSafAbortError(requestId, detail)))
                return
            }
            finish(() => reject(new AndroidSafDestinationError(
                requestId,
                detail.code ?? 'destination-write-failed',
                detail.warningCodes,
                detail.message ?? 'Android SAF destination copy failed',
            )))
        }
        const onProgress = (event: Event) => {
            const detail = (event as CustomEvent<AndroidSafProgress>).detail
            if (
                detail?.requestId === requestId &&
                detail.operation === 'destination-copy'
            ) {
                request.onProgress?.(detail)
            }
        }
        dependencies.addEventListener(DESTINATION_EVENT, onEvent)
        if (request.onProgress) {
            dependencies.addEventListener(PROGRESS_EVENT, onProgress)
        }
        request.signal?.addEventListener('abort', onAbort, { once: true })
        try {
            dependencies.bridge.copyExport(
                requestId,
                request.sourcePath,
                request.suggestedName,
            )
        }
        catch (error) {
            finish(() => reject(error))
        }
    })
}

export function pickAndroidContentSource(
    options: AndroidSafSourcePickerOptions = {},
    dependencies: AndroidSafSourcePickerDependencies = productionDependencies,
): Promise<NativeFileJobSource | null> {
    return pickAndroidSpoolSource(
        {
            eventName: 'risu-android-content-source-picked',
            pickMethod: 'pickContentSource',
            pickerUnavailableMessage: 'Android content picker is unavailable',
            acceptExtension: [
                '.json',
                '.png',
                '.charx',
                '.jpg',
                '.jpeg',
                '.risum',
                '.lorebook',
            ],
            cancelMessage: 'Android content selection was cancelled',
        },
        options,
        dependencies,
    )
}
