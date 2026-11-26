import { joinAbortSignals } from '../../abortSignals'
import type { requestDataResponse, StreamResponseChunk } from './request'

export interface OutputPluginRequestBoundary {
    signal: AbortSignal
    open(): Promise<void>
    close(failure?: { error: unknown }): Promise<void>
}

export async function runOutputPluginRequest(
    boundary: OutputPluginRequestBoundary,
    caller: AbortSignal | undefined,
    request: (signal: AbortSignal, beforePlugin: () => Promise<void>) => Promise<requestDataResponse>,
    collect?: (stream: ReadableStream<StreamResponseChunk>) => Promise<string>,
): Promise<requestDataResponse> {
    const admissionFailure = new AbortController()
    const { signal, dispose } = joinAbortSignals([boundary.signal, caller, admissionFailure.signal])
    let opened = false
    let finished = false
    let failure: { error: unknown } | undefined
    let onAbort: () => void
    const cancellation = new Promise<never>((_, reject) => {
        onAbort = () => reject(signal.reason)
        signal.addEventListener('abort', onAbort, { once: true })
    })
    async function beforePlugin() {
        signal.throwIfAborted()
        if (opened) return
        try {
            await boundary.open()
        } catch (error) {
            // Provider dispatch converts ordinary provider errors to retryable results.
            admissionFailure.abort(error)
            throw error
        }
        // Cancellation can finish the caller while the save that opens admission is settling.
        if (finished) {
            await boundary.close({ error: signal.reason })
            signal.throwIfAborted()
        }
        opened = true
        signal.throwIfAborted()
    }
    async function run(): Promise<requestDataResponse> {
        signal.throwIfAborted()
        const response = await request(signal, beforePlugin)
        if (response.type === 'streaming' && signal.aborted) {
            void response.result.cancel(signal.reason).catch(() => {})
        }
        signal.throwIfAborted()
        if (!opened) {
            // Script calls that consume ordinary streams still stop on owner loss.
            if (collect && response.type === 'streaming') {
                return { ...response, result: response.result.pipeThrough(new TransformStream(), { signal: boundary.signal }) }
            }
            return response
        }
        if (response.type === 'fail') throw new Error(response.result)
        if (response.type !== 'streaming') return response
        if (!collect) {
            // Legacy script calls reject a streaming response without reading it.
            void response.result.cancel().catch(() => {})
            return response
        }
        const stream = response.result.pipeThrough(new TransformStream<StreamResponseChunk, StreamResponseChunk>(), { signal })
        return { ...response, type: 'success', result: await collect(stream) }
    }
    try {
        const result = await Promise.race([run(), cancellation])
        signal.throwIfAborted()
        return result
    } catch (error) {
        failure = { error }
        throw error
    } finally {
        finished = true
        signal.removeEventListener('abort', onAbort)
        dispose()
        if (opened) await boundary.close(failure)
    }
}
