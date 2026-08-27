import type { NativeFileJobSource } from './nativeFileJobs'

const SPOOL_EVENT = 'risu-android-spool-ready'
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

export async function consumeAndroidSpoolBatch(
    batch: AndroidSpoolBatch,
    consumer: AndroidSpoolConsumer,
): Promise<void> {
    for (const failure of batch.failures) consumer.failed?.(failure)
    for (const source of batch.ready) {
        if (!source.displayName.toLocaleLowerCase('en-US').endsWith('.risudat')) {
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
    sourceKind?: 'risuSave' | 'screenshot'
    state: 'succeeded' | 'failed' | 'cancelled'
    bytes?: number | null
    code?: string | null
    message?: string | null
    warningCodes: string[]
}

export interface AndroidSafJavascriptBridge {
    copyExport(
        requestId: string,
        sourcePath: string,
        suggestedName: string,
    ): void
    cancelExport?(requestId: string): boolean | void
    cancelSource?(requestId: string): void
    discardSource?(token: string): boolean
    getActiveSourceRequestIds?(): string
    getExportStatus?(): string | null
    acknowledgeExport?(requestId: string): boolean
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

export function getAndroidSafExportStatus(
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): string | null {
    return bridge.getExportStatus?.() ?? null
}

export function cancelAndroidSafSource(
    requestId: string,
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): void {
    bridge.cancelSource?.(requestId)
}

export function discardAndroidSafSource(
    token: string,
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): boolean {
    return bridge.discardSource?.(token) === true
}

export function getActiveAndroidSafSourceRequestIds(
    bridge: AndroidSafJavascriptBridge = productionBridge(),
): string[] {
    const encoded = bridge.getActiveSourceRequestIds?.()
    if (!encoded) return []
    try {
        const requestIds: unknown = JSON.parse(encoded)
        if (!Array.isArray(requestIds)) return []
        return requestIds.filter((value): value is string =>
            typeof value === 'string'
            && /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(value))
            .slice(0, 16)
    }
    catch {
        return []
    }
}

export function listenAndroidSafProgress(
    listener: (progress: AndroidSafProgress) => void,
    dependencies: Pick<AndroidSafDestinationDependencies, 'addEventListener' | 'removeEventListener'> = productionDependencies,
): () => void {
    const onProgress = (event: Event) => {
        const detail = (event as CustomEvent<AndroidSafProgress>).detail
        if (detail) listener(detail)
    }
    dependencies.addEventListener(PROGRESS_EVENT, onProgress)
    return () => dependencies.removeEventListener(PROGRESS_EVENT, onProgress)
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
