import { fetch as tauriFetch } from '@tauri-apps/plugin-http'

/**
 * Silence allowed between response events before a native request is released.
 * The window measures inactivity rather than total duration, so a slow but still
 * streaming generation is never cut off while a dead connection is still dropped.
 */
export const DEFAULT_IDLE_TIMEOUT_MS = 600_000

export type TauriHttpStreamFinish = () => void

export interface TauriHttpStreamOptions {
    url: string
    method: string
    headers: { [key: string]: string }
    body?: Uint8Array
    signal?: AbortSignal
    /** Inactivity window in milliseconds. A nonpositive value disables the timer. */
    idleTimeoutMs?: number
    onChunk?: (chunk: Uint8Array) => void
    onFinish?: TauriHttpStreamFinish
}

function createRequestLifecycle(options: TauriHttpStreamOptions) {
    const controller = new AbortController()
    let timer: ReturnType<typeof setTimeout> | undefined
    let signalCleaned = false
    let finished = false
    let releaseBody: (() => void) | undefined

    const cleanupSignal = () => {
        if (signalCleaned) return
        signalCleaned = true
        if (timer !== undefined) clearTimeout(timer)
        options.signal?.removeEventListener('abort', abortFromCaller)
    }
    const finish = () => {
        if (finished) return
        finished = true
        cleanupSignal()
        releaseBody?.()
        options.onFinish?.()
    }
    const abortFromCaller = () => {
        controller.abort(options.signal?.reason)
        finish()
    }

    if (options.signal?.aborted) {
        controller.abort(options.signal.reason)
    }
    else if (options.signal) {
        options.signal.addEventListener('abort', abortFromCaller, { once: true })
    }
    const idleTimeoutMs = options.idleTimeoutMs ?? DEFAULT_IDLE_TIMEOUT_MS
    const armed = idleTimeoutMs > 0 && !controller.signal.aborted
    let deadline = armed ? Date.now() + idleTimeoutMs : 0
    const onIdleDeadline = () => {
        const remaining = deadline - Date.now()
        // Progress landed while this timer was pending, so wait out the new window.
        if (remaining > 0) {
            timer = setTimeout(onIdleDeadline, remaining)
            return
        }
        controller.abort(new DOMException('The operation timed out', 'TimeoutError'))
        finish()
    }
    if (armed) {
        timer = setTimeout(onIdleDeadline, idleTimeoutMs)
    }

    return {
        signal: controller.signal,
        finish,
        /** Restarts the inactivity window whenever the request makes progress. */
        noteProgress() {
            if (armed && !finished) deadline = Date.now() + idleTimeoutMs
        },
        /** Lets an aborted or timed out request release the native response body. */
        onRelease(release: () => void) {
            if (finished) release()
            else releaseBody = release
        },
    }
}

export async function fetchTauriHttpStream(options: TauriHttpStreamOptions): Promise<Response> {
    const lifecycle = createRequestLifecycle(options)
    let response: Response
    try {
        response = await tauriFetch(options.url, {
            method: options.method,
            headers: options.headers,
            body: options.method === 'GET' || options.method === 'HEAD'
                ? undefined
                : options.body as unknown as BodyInit,
            signal: lifecycle.signal,
        })
    }
    catch (error) {
        lifecycle.finish()
        throw error
    }
    lifecycle.noteProgress()

    if (response.body === null) {
        lifecycle.finish()
        return response
    }

    const reader = response.body.getReader()
    lifecycle.onRelease(() => void reader.cancel().catch(() => undefined))
    const body = new ReadableStream<Uint8Array>({
        async pull(controller) {
            try {
                const result = await reader.read()
                if (result.done) {
                    lifecycle.finish()
                    controller.close()
                    return
                }
                lifecycle.noteProgress()
                options.onChunk?.(result.value)
                controller.enqueue(result.value)
            }
            catch (error) {
                lifecycle.finish()
                controller.error(error)
            }
        },
        async cancel(reason) {
            try {
                await reader.cancel(reason)
            }
            finally {
                lifecycle.finish()
            }
        },
    }, { highWaterMark: 0 })
    const wrapped = new Response(body, {
        status: response.status,
        statusText: response.statusText,
    })
    Object.defineProperty(wrapped, 'url', { value: response.url, writable: false })
    Object.defineProperty(wrapped, 'headers', { value: response.headers, writable: false })
    return wrapped
}
