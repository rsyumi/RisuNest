import type { RegexExecutionPlan, RegexExecutionResult } from './regexExecutionPlan'

export const DEFAULT_REGEX_WORKER_TIMEOUT_MS = 2_000

export type RegexWorkerPlanEntry = [
    sourceIndex: number,
    pattern: string,
    replacement: string,
    flags: string,
]

export type RegexWorkerRequest = {
    type: 'register'
    revision: number
    entries: RegexWorkerPlanEntry[]
} | {
    type: 'execute'
    id: number
    revision: number
    input: string
}

export type RegexWorkerResponse = {
    type: 'result'
    id: number
    data: string
    errors: [sourceIndex: number, message: string][]
} | {
    type: 'error'
    id: number
    message: string
}

export interface RegexWorkerLike {
    postMessage(message: RegexWorkerRequest): void
    terminate(): void
    addEventListener(type: 'message' | 'error', listener: EventListener): void
    removeEventListener(type: 'message' | 'error', listener: EventListener): void
}

export interface RegexWorkerExecuteOptions {
    signal?: AbortSignal
    timeoutMs?: number
}

interface PendingRequest {
    revision: number
    signal?: AbortSignal
    abortListener?: () => void
    timeout: ReturnType<typeof setTimeout>
    resolve: (result: RegexExecutionResult) => void
    reject: (error: unknown) => void
}

export class RegexExecutionTimeoutError extends Error {
    readonly category = 'regex_timeout'

    constructor(readonly revision: number) {
        super(`Regex Worker timed out for plan revision ${revision}`)
        this.name = 'RegexExecutionTimeoutError'
    }
}

function createAbortError(signal: AbortSignal): unknown {
    if (signal.reason !== undefined) {
        return signal.reason
    }
    return new DOMException('The operation was aborted', 'AbortError')
}

function createModuleWorker(): RegexWorkerLike {
    return new Worker(new URL('./regexWorker.ts', import.meta.url), { type: 'module' }) as unknown as RegexWorkerLike
}

export class RegexWorkerClient {
    private worker: RegexWorkerLike | undefined
    private registeredRevision: number | undefined
    private readonly pending = new Map<number, PendingRequest>()
    private nextRequestId = 1

    constructor(private readonly workerFactory: () => RegexWorkerLike = createModuleWorker) {}

    execute(
        plan: RegexExecutionPlan,
        input: string,
        options: RegexWorkerExecuteOptions = {},
    ): Promise<RegexExecutionResult> {
        if (options.signal?.aborted) {
            return Promise.reject(createAbortError(options.signal))
        }

        const worker = this.ensureWorker()
        if (this.registeredRevision !== plan.revision) {
            try {
                worker.postMessage({
                    type: 'register',
                    revision: plan.revision,
                    entries: plan.entries.map((entry) => [
                        entry.sourceIndex,
                        entry.pattern,
                        entry.replacement,
                        entry.flags,
                    ]),
                })
                this.registeredRevision = plan.revision
            }
            catch (error) {
                this.replaceWorker(error)
                return Promise.reject(error)
            }
        }

        const id = this.nextRequestId++
        return new Promise<RegexExecutionResult>((resolve, reject) => {
            const timeout = setTimeout(() => {
                this.replaceWorker(new RegexExecutionTimeoutError(plan.revision))
            }, options.timeoutMs ?? DEFAULT_REGEX_WORKER_TIMEOUT_MS)
            const pending: PendingRequest = {
                revision: plan.revision,
                signal: options.signal,
                timeout,
                resolve,
                reject,
            }
            if (options.signal !== undefined) {
                pending.abortListener = () => {
                    this.replaceWorker(createAbortError(options.signal!))
                }
                options.signal.addEventListener('abort', pending.abortListener, { once: true })
            }
            this.pending.set(id, pending)

            try {
                worker.postMessage({ type: 'execute', id, revision: plan.revision, input })
            }
            catch (error) {
                this.replaceWorker(error)
            }
        })
    }

    private ensureWorker(): RegexWorkerLike {
        if (this.worker !== undefined) {
            return this.worker
        }

        const worker = this.workerFactory()
        const messageListener: EventListener = (event) => {
            if (this.worker !== worker) {
                return
            }
            this.handleResponse((event as MessageEvent<RegexWorkerResponse>).data)
        }
        const errorListener: EventListener = (event) => {
            if (this.worker !== worker) {
                return
            }
            const errorEvent = event as ErrorEvent
            this.replaceWorker(errorEvent.error ?? new Error(errorEvent.message || 'Regex Worker failed'))
        }
        worker.addEventListener('message', messageListener)
        worker.addEventListener('error', errorListener)
        this.worker = worker
        this.workerListeners = { worker, messageListener, errorListener }
        return worker
    }

    private workerListeners: {
        worker: RegexWorkerLike
        messageListener: EventListener
        errorListener: EventListener
    } | undefined

    private handleResponse(response: RegexWorkerResponse): void {
        const pending = this.pending.get(response.id)
        if (pending === undefined) {
            return
        }

        this.pending.delete(response.id)
        this.cleanupPending(pending)
        if (response.type === 'error') {
            pending.reject(new Error(response.message))
            return
        }
        pending.resolve({
            data: response.data,
            errors: response.errors.map(([sourceIndex, message]) => ({
                sourceIndex,
                error: new Error(message),
            })),
        })
    }

    private replaceWorker(error: unknown): void {
        const listeners = this.workerListeners
        this.worker = undefined
        this.workerListeners = undefined
        this.registeredRevision = undefined
        if (listeners !== undefined) {
            listeners.worker.removeEventListener('message', listeners.messageListener)
            listeners.worker.removeEventListener('error', listeners.errorListener)
            listeners.worker.terminate()
        }

        const requests = [...this.pending.values()]
        this.pending.clear()
        for (const pending of requests) {
            this.cleanupPending(pending)
            pending.reject(error)
        }
    }

    private cleanupPending(pending: PendingRequest): void {
        clearTimeout(pending.timeout)
        if (pending.signal !== undefined && pending.abortListener !== undefined) {
            pending.signal.removeEventListener('abort', pending.abortListener)
        }
    }
}

let sharedRegexWorkerClient: RegexWorkerClient | undefined

export function getSharedRegexWorkerClient(): RegexWorkerClient {
    sharedRegexWorkerClient ??= new RegexWorkerClient()
    return sharedRegexWorkerClient
}

export function isRegexWorkerAvailable(): boolean {
    return typeof Worker !== 'undefined'
}
