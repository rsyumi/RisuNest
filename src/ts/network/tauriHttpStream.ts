import { fetch as tauriFetch } from '@tauri-apps/plugin-http'

export type TauriHttpStreamFinish = () => void

export interface TauriHttpStreamOptions {
    url: string
    method: 'POST' | 'GET' | 'PUT' | 'DELETE'
    headers: { [key: string]: string }
    body?: Uint8Array
    signal?: AbortSignal
    requestTimeoutMs?: number
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
    if (options.requestTimeoutMs !== undefined && options.requestTimeoutMs > 0 && !controller.signal.aborted) {
        timer = setTimeout(() => {
            controller.abort(new DOMException('The operation timed out', 'TimeoutError'))
            finish()
        }, options.requestTimeoutMs)
    }

    return {
        signal: controller.signal,
        finish,
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
            body: options.method === 'GET' || options.method === 'DELETE'
                ? undefined
                : options.body as unknown as BodyInit,
            signal: lifecycle.signal,
        })
    }
    catch (error) {
        lifecycle.finish()
        throw error
    }

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
