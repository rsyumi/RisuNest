import type { NativeFileJobSource } from './nativeFileJobs'

const DESTINATION_EVENT = 'risu-android-saf-destination'

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
    ready: AndroidSpoolReady[]
    failures: AndroidSpoolFailure[]
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
}

export interface AndroidSafDestinationResult {
    bytes: number
    warningCodes: string[]
}

export interface AndroidSafDestinationEvent {
    requestId: string
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
    cancelExport?(requestId: string): void
}

export interface AndroidSafDestinationDependencies {
    createRequestId(): string
    bridge: AndroidSafJavascriptBridge
    addEventListener(name: string, listener: (event: Event) => void): void
    removeEventListener(name: string, listener: (event: Event) => void): void
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
        readonly code: string,
        readonly warningCodes: string[],
        message: string,
    ) {
        super(message)
        this.name = 'AndroidSafDestinationError'
    }
}

export function copyNativeExportToAndroidSaf(
    request: AndroidSafDestinationRequest,
    dependencies: AndroidSafDestinationDependencies = productionDependencies,
): Promise<AndroidSafDestinationResult> {
    if (request.signal?.aborted) {
        return Promise.reject(new DOMException('Android SAF export was cancelled', 'AbortError'))
    }
    const requestId = dependencies.createRequestId()
    return new Promise((resolve, reject) => {
        let settled = false
        const cleanup = () => {
            request.signal?.removeEventListener('abort', onAbort)
            dependencies.removeEventListener(DESTINATION_EVENT, onEvent)
        }
        const finish = (callback: () => void) => {
            if (settled) return
            settled = true
            cleanup()
            callback()
        }
        const onAbort = () => {
            dependencies.bridge.cancelExport?.(requestId)
            finish(() => reject(new DOMException('Android SAF export was cancelled', 'AbortError')))
        }
        const onEvent = (event: Event) => {
            const detail = (event as CustomEvent<AndroidSafDestinationEvent>).detail
            if (!detail || detail.requestId !== requestId) return
            if (detail.state === 'succeeded' && typeof detail.bytes === 'number') {
                finish(() => resolve({
                    bytes: detail.bytes as number,
                    warningCodes: detail.warningCodes,
                }))
                return
            }
            if (detail.state === 'cancelled') {
                finish(() => reject(new DOMException('Android SAF export was cancelled', 'AbortError')))
                return
            }
            finish(() => reject(new AndroidSafDestinationError(
                detail.code ?? 'destination-write-failed',
                detail.warningCodes,
                detail.message ?? 'Android SAF destination copy failed',
            )))
        }
        dependencies.addEventListener(DESTINATION_EVENT, onEvent)
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
