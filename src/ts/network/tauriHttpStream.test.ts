import { afterEach, describe, expect, test, vi } from 'vitest'

const { pluginFetch } = vi.hoisted(() => ({
    pluginFetch: vi.fn(),
}))

vi.mock('@tauri-apps/plugin-http', () => ({
    fetch: pluginFetch,
}))

import { DEFAULT_IDLE_TIMEOUT_MS, fetchTauriHttpStream } from './tauriHttpStream'

function pluginResponse(
    body: ReadableStream<Uint8Array> | null,
    init: ResponseInit & { url?: string } = {},
) {
    const response = new Response(body, init)
    if (init.url) {
        Object.defineProperty(response, 'url', { value: init.url })
    }
    return response
}

function trackedSignal() {
    const controller = new AbortController()
    const add = vi.spyOn(controller.signal, 'addEventListener')
    const remove = vi.spyOn(controller.signal, 'removeEventListener')
    return { controller, add, remove }
}

afterEach(() => {
    vi.useRealTimers()
    vi.restoreAllMocks()
    pluginFetch.mockReset()
})

describe('fetchTauriHttpStream', () => {
    test.each(['DELETE', 'PATCH'])(
        'preserves %s request bodies',
        async (method) => {
            pluginFetch.mockResolvedValue(new Response(null, { status: 204 }))
            const body = new TextEncoder().encode('synthetic payload')
            await fetchTauriHttpStream({
                url: 'https://api.example.invalid:8443/',
                method,
                headers: {},
                body,
            })
            expect(pluginFetch).toHaveBeenCalledWith(
                'https://api.example.invalid:8443/',
                expect.objectContaining({ method, body }),
            )
        },
    )

    test('preserves request bytes, response metadata, and chunk boundaries', async () => {
        const chunks = [new Uint8Array([4, 5]), new Uint8Array([6, 7, 8])]
        const upstream = new ReadableStream<Uint8Array>({
            start(controller) {
                for (const chunk of chunks) controller.enqueue(chunk)
                controller.close()
            },
        })
        const response = pluginResponse(upstream, {
            status: 207,
            statusText: 'Multi-Status',
            headers: [['X-Mixed-Case', 'first'], ['x-repeat-safe', 'one, two']],
            url: 'https://resolved.example/stream',
        })
        pluginFetch.mockResolvedValue(response)
        const body = new Uint8Array([0, 255, 1, 128])

        const result = await fetchTauriHttpStream({
            url: 'https://request.example/path',
            method: 'PUT',
            headers: { 'X-Request': 'value' },
            body,
        })

        expect(pluginFetch).toHaveBeenCalledOnce()
        const [input, init] = pluginFetch.mock.calls[0]
        expect(input).toBe('https://request.example/path')
        expect(init).toMatchObject({ method: 'PUT', headers: { 'X-Request': 'value' } })
        expect(init.body).toBe(body)
        expect([...init.body]).toEqual([0, 255, 1, 128])
        expect(result.status).toBe(207)
        expect(result.statusText).toBe('Multi-Status')
        expect(result.url).toBe('https://resolved.example/stream')
        expect([...result.headers.entries()]).toEqual([...response.headers.entries()])

        const reader = result.body!.getReader()
        expect(await reader.read()).toEqual({ done: false, value: chunks[0] })
        expect(await reader.read()).toEqual({ done: false, value: chunks[1] })
        expect(await reader.read()).toEqual({ done: true, value: undefined })
    })

    test('allows one plugin-buffered chunk without wrapper or logging lookahead', async () => {
        let pulls = 0
        const chunks = [
            new Uint8Array([1]),
            new Uint8Array([2]),
            new Uint8Array([3]),
            new Uint8Array([4]),
        ]
        const upstream = new ReadableStream<Uint8Array>({
            pull(controller) {
                const chunk = chunks[pulls]
                pulls += 1
                controller.enqueue(chunk)
            },
        })
        pluginFetch.mockResolvedValue(pluginResponse(upstream))
        const onChunk = vi.fn()

        const response = await fetchTauriHttpStream({
            url: 'https://example.test/stream',
            method: 'GET',
            headers: {},
            onChunk,
        })
        expect(pulls).toBe(1)

        const reader = response.body!.getReader()
        expect(await reader.read()).toEqual({ done: false, value: chunks[0] })
        expect(onChunk).toHaveBeenCalledExactlyOnceWith(chunks[0])
        await Promise.resolve()
        await Promise.resolve()
        expect(pulls).toBeLessThanOrEqual(2)
        const pullsAfterFirstRead = pulls
        await Promise.resolve()
        await Promise.resolve()
        expect(pulls).toBe(pullsAfterFirstRead)

        expect(await reader.read()).toEqual({ done: false, value: chunks[1] })
        expect(onChunk).toHaveBeenNthCalledWith(2, chunks[1])
        await Promise.resolve()
        expect(pulls).toBe(pullsAfterFirstRead + 1)
        const pullsAfterSecondRead = pulls
        await Promise.resolve()
        await Promise.resolve()
        expect(pulls).toBe(pullsAfterSecondRead)

        expect(await reader.read()).toEqual({ done: false, value: chunks[2] })
        expect(onChunk).toHaveBeenNthCalledWith(3, chunks[2])
        await Promise.resolve()
        expect(pulls).toBe(pullsAfterSecondRead + 1)
        await reader.cancel()
    })

    test('restarts the inactivity window after each chunk', async () => {
        vi.useFakeTimers()
        const caller = trackedSignal()
        const cancellationError = new Error('plugin timeout cancellation')
        let effectiveSignal: AbortSignal | undefined
        let release: (() => void) | undefined
        pluginFetch.mockImplementation(async (_url, init) => {
            effectiveSignal = init?.signal
            return pluginResponse(new ReadableStream<Uint8Array>({
                start(controller) {
                    effectiveSignal!.addEventListener('abort', () => controller.error(cancellationError), { once: true })
                },
                pull(controller) {
                    return new Promise<void>((resolve) => {
                        release = () => {
                            controller.enqueue(new Uint8Array([1]))
                            resolve()
                        }
                    })
                },
            }, { highWaterMark: 0 }))
        })
        const onFinish = vi.fn()

        const response = await fetchTauriHttpStream({
            url: 'https://example.test/timeout',
            method: 'GET',
            headers: {},
            signal: caller.controller.signal,
            idleTimeoutMs: 25,
            onFinish,
        })
        expect(effectiveSignal).not.toBe(caller.controller.signal)
        expect(effectiveSignal?.aborted).toBe(false)
        const reader = response.body!.getReader()

        // Three windows of steady traffic must not end a request that keeps moving.
        for (let round = 0; round < 3; round += 1) {
            const pending = reader.read()
            await vi.advanceTimersByTimeAsync(20)
            release!()
            expect(await pending).toMatchObject({ done: false, value: new Uint8Array([1]) })
            expect(effectiveSignal?.aborted).toBe(false)
        }

        const stalled = reader.read()
        stalled.catch(() => undefined)
        await vi.advanceTimersByTimeAsync(24)
        expect(effectiveSignal?.aborted).toBe(false)
        await vi.advanceTimersByTimeAsync(1)
        expect(effectiveSignal?.aborted).toBe(true)
        expect((effectiveSignal?.reason as DOMException).name).toBe('TimeoutError')
        await expect(stalled).rejects.toBe(cancellationError)
        expect(vi.getTimerCount()).toBe(0)
        expect(caller.remove).toHaveBeenCalledWith('abort', expect.any(Function))
        expect(onFinish).toHaveBeenCalledOnce()
    })

    test('defaults the inactivity window to ten minutes when unspecified', async () => {
        vi.useFakeTimers()
        const cancellationError = new Error('stalled connection cancelled')
        let effectiveSignal: AbortSignal | undefined
        pluginFetch.mockImplementation(async (_url, init) => {
            effectiveSignal = init?.signal
            return pluginResponse(new ReadableStream<Uint8Array>({
                start(controller) {
                    effectiveSignal!.addEventListener('abort', () => controller.error(cancellationError), { once: true })
                },
            }, { highWaterMark: 0 }))
        })
        const onFinish = vi.fn()

        const response = await fetchTauriHttpStream({
            url: 'https://example.test/stalled',
            method: 'GET',
            headers: {},
            onFinish,
        })
        const reader = response.body!.getReader()
        const pendingRead = reader.read()
        pendingRead.catch(() => undefined)

        await vi.advanceTimersByTimeAsync(DEFAULT_IDLE_TIMEOUT_MS - 1)
        expect(effectiveSignal?.aborted).toBe(false)
        await vi.advanceTimersByTimeAsync(1)
        expect(effectiveSignal?.aborted).toBe(true)
        expect((effectiveSignal?.reason as DOMException).name).toBe('TimeoutError')
        await expect(pendingRead).rejects.toBe(cancellationError)
        expect(vi.getTimerCount()).toBe(0)
        expect(onFinish).toHaveBeenCalledOnce()
    })

    test('a nonpositive explicit timeout disables the inactivity timer', async () => {
        vi.useFakeTimers()
        pluginFetch.mockImplementation(async () => pluginResponse(new ReadableStream<Uint8Array>({
            pull(controller) {
                controller.enqueue(new Uint8Array([1]))
            },
        }, { highWaterMark: 0 })))

        const response = await fetchTauriHttpStream({
            url: 'https://example.test/no-timeout',
            method: 'GET',
            headers: {},
            idleTimeoutMs: 0,
        })
        expect(vi.getTimerCount()).toBe(0)

        await vi.advanceTimersByTimeAsync(DEFAULT_IDLE_TIMEOUT_MS + 1)
        const reader = response.body!.getReader()
        expect(await reader.read()).toMatchObject({ done: false, value: new Uint8Array([1]) })
        await reader.cancel()
    })

    test('propagates caller abort before headers and cleans up once', async () => {
        vi.useFakeTimers()
        for (const alreadyAborted of [false, true]) {
            const caller = trackedSignal()
            const cancellationError = new Error(`cancelled ${alreadyAborted}`)
            if (alreadyAborted) caller.controller.abort()
            pluginFetch.mockImplementationOnce((_url, init) => new Promise((_resolve, reject) => {
                const signal = init?.signal as AbortSignal
                if (signal.aborted) {
                    reject(cancellationError)
                    return
                }
                signal.addEventListener('abort', () => reject(cancellationError), { once: true })
            }))
            const onFinish = vi.fn()
            const pending = fetchTauriHttpStream({
                url: 'https://example.test/pending',
                method: 'POST',
                headers: {},
                body: new Uint8Array([3]),
                signal: caller.controller.signal,
                idleTimeoutMs: 100,
                onFinish,
            })
            if (!alreadyAborted) caller.controller.abort()

            await expect(pending).rejects.toBe(cancellationError)
            const effectiveSignal = pluginFetch.mock.calls.at(-1)![1].signal as AbortSignal
            expect(effectiveSignal.aborted).toBe(true)
            expect(vi.getTimerCount()).toBe(0)
            if (!alreadyAborted) {
                expect(caller.remove).toHaveBeenCalledWith('abort', expect.any(Function))
            }
            expect(onFinish).toHaveBeenCalledOnce()
        }
    })

    test('propagates caller abort after the first chunk without pulling later chunks', async () => {
        const caller = trackedSignal()
        const cancellationError = new Error('plugin body cancelled')
        let pulls = 0
        pluginFetch.mockImplementation(async (_url, init) => {
            const signal = init?.signal as AbortSignal
            return pluginResponse(new ReadableStream<Uint8Array>({
                start(controller) {
                    signal.addEventListener('abort', () => controller.error(cancellationError), { once: true })
                },
                pull(controller) {
                    pulls += 1
                    controller.enqueue(new Uint8Array([pulls]))
                },
            }, { highWaterMark: 0 }))
        })
        const onFinish = vi.fn()
        const response = await fetchTauriHttpStream({
            url: 'https://example.test/abort-body',
            method: 'GET',
            headers: {},
            signal: caller.controller.signal,
            onFinish,
        })
        const reader = response.body!.getReader()
        expect(await reader.read()).toMatchObject({ value: new Uint8Array([1]) })

        caller.controller.abort()
        await expect(reader.read()).rejects.toBe(cancellationError)
        expect(pulls).toBe(1)
        expect(onFinish).toHaveBeenCalledOnce()
        expect(caller.remove).toHaveBeenCalledWith('abort', expect.any(Function))
    })

    test('releases the native body when the caller aborts an unread response', async () => {
        const caller = trackedSignal()
        const cancel = vi.fn(async () => undefined)
        pluginFetch.mockImplementation(async () => pluginResponse(new ReadableStream<Uint8Array>({
            pull(controller) {
                controller.enqueue(new Uint8Array([1]))
            },
            cancel,
        }, { highWaterMark: 0 })))
        const onFinish = vi.fn()

        await fetchTauriHttpStream({
            url: 'https://example.test/discarded',
            method: 'GET',
            headers: {},
            signal: caller.controller.signal,
            onFinish,
        })
        caller.controller.abort()
        await Promise.resolve()

        expect(cancel).toHaveBeenCalledOnce()
        expect(onFinish).toHaveBeenCalledOnce()
    })

    test('forwards downstream cancellation and finalizes once', async () => {
        vi.useFakeTimers()
        const caller = trackedSignal()
        const cancel = vi.fn()
        let pulls = 0
        const upstream = new ReadableStream<Uint8Array>({
            pull(controller) {
                pulls += 1
                controller.enqueue(new Uint8Array([pulls]))
            },
            cancel,
        }, { highWaterMark: 0 })
        pluginFetch.mockResolvedValue(pluginResponse(upstream))
        const onFinish = vi.fn()
        const response = await fetchTauriHttpStream({
            url: 'https://example.test/cancel',
            method: 'GET',
            headers: {},
            signal: caller.controller.signal,
            idleTimeoutMs: 1000,
            onFinish,
        })
        const reader = response.body!.getReader()
        await reader.read()
        const reason = { reason: 'consumer stopped' }
        await reader.cancel(reason)

        expect(cancel).toHaveBeenCalledExactlyOnceWith(reason)
        expect(pulls).toBe(1)
        expect(vi.getTimerCount()).toBe(0)
        expect(caller.remove).toHaveBeenCalledWith('abort', expect.any(Function))
        expect(onFinish).toHaveBeenCalledOnce()
    })

    test('preserves a body read error and response metadata', async () => {
        const sentinel = new Error('read failed')
        pluginFetch.mockResolvedValue(pluginResponse(new ReadableStream<Uint8Array>({
            pull() {
                throw sentinel
            },
        }, { highWaterMark: 0 }), { status: 206, statusText: 'Partial Content' }))
        const onFinish = vi.fn()
        const response = await fetchTauriHttpStream({
            url: 'https://example.test/error',
            method: 'GET',
            headers: {},
            onFinish,
        })

        expect(response.status).toBe(206)
        expect(response.statusText).toBe('Partial Content')
        await expect(response.body!.getReader().read()).rejects.toBe(sentinel)
        expect(onFinish).toHaveBeenCalledOnce()
    })

    test('preserves a fetch error and finalizes once', async () => {
        vi.useFakeTimers()
        const caller = trackedSignal()
        const sentinel = new Error('fetch failed')
        pluginFetch.mockRejectedValue(sentinel)
        const onFinish = vi.fn()

        await expect(fetchTauriHttpStream({
            url: 'https://example.test/fetch-error',
            method: 'GET',
            headers: {},
            signal: caller.controller.signal,
            idleTimeoutMs: 1000,
            onFinish,
        })).rejects.toBe(sentinel)

        expect(vi.getTimerCount()).toBe(0)
        expect(caller.remove).toHaveBeenCalledWith('abort', expect.any(Function))
        expect(onFinish).toHaveBeenCalledOnce()
    })

    test('returns a null-body response unchanged and finalizes immediately', async () => {
        const response = pluginResponse(null, { status: 204 })
        pluginFetch.mockResolvedValue(response)
        const onFinish = vi.fn()

        const result = await fetchTauriHttpStream({
            url: 'https://example.test/no-content',
            method: 'DELETE',
            headers: {},
            body: new Uint8Array([1, 2, 3]),
            onFinish,
        })

        expect(result).toBe(response)
        expect(result.body).toBeNull()
        expect(pluginFetch.mock.calls[0][1].body).toEqual(new Uint8Array([1, 2, 3]))
        expect(onFinish).toHaveBeenCalledOnce()
    })

    test('finalizes once after normal EOF and remains finalized after cancellation', async () => {
        vi.useFakeTimers()
        const caller = trackedSignal()
        pluginFetch.mockResolvedValue(pluginResponse(new ReadableStream<Uint8Array>({
            start(controller) {
                controller.enqueue(new Uint8Array([9]))
                controller.close()
            },
        }, { highWaterMark: 0 })))
        const onFinish = vi.fn()
        const response = await fetchTauriHttpStream({
            url: 'https://example.test/complete',
            method: 'GET',
            headers: {},
            signal: caller.controller.signal,
            idleTimeoutMs: 1000,
            onFinish,
        })

        const reader = response.body!.getReader()
        expect(await reader.read()).toMatchObject({ done: false, value: new Uint8Array([9]) })
        expect(await reader.read()).toEqual({ done: true, value: undefined })
        await reader.cancel('after completion')
        expect(vi.getTimerCount()).toBe(0)
        expect(caller.remove).toHaveBeenCalledWith('abort', expect.any(Function))
        expect(onFinish).toHaveBeenCalledOnce()
    })
})
