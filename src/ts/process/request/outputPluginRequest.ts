import { joinAbortSignals } from '../../abortSignals'
import type { requestDataResponse, StreamResponseChunk } from './request'

export type ScriptStreamCollector = (stream: ReadableStream<StreamResponseChunk>) =>
    Promise<Extract<requestDataResponse, { result: string }>>

export interface OutputPluginRequestBoundary {
    signal: AbortSignal
    open(): Promise<void>
    close(failure?: { error: unknown }): Promise<void>
}

export async function runOutputPluginRequest(
    boundary: OutputPluginRequestBoundary,
    caller: AbortSignal | undefined,
    request: (signal: AbortSignal, beforePlugin: () => Promise<void>) => Promise<requestDataResponse>,
    collect?: ScriptStreamCollector,
): Promise<requestDataResponse> {
    const admissionFailure = new AbortController()
    const { signal, dispose } = joinAbortSignals([boundary.signal, caller, admissionFailure.signal])
    let opening: Promise<void> | undefined
    let opened = false
    let finished = false
    let onAbort: () => void
    const cancellation = new Promise<never>((_, reject) => {
        onAbort = () => reject(signal.reason)
        signal.addEventListener('abort', onAbort, { once: true })
    })
    async function open() {
        try {
            await boundary.open()
        } catch (error) {
            // Provider dispatch converts ordinary provider errors to retryable results.
            admissionFailure.abort(error)
            throw error
        }
        // A cancelled caller can finish while its checkpoint is still saving.
        if (finished) await close()
        else opened = true
    }
    function close() {
        return boundary.close(signal.aborted ? { error: signal.reason } : undefined)
    }
    function requireRunning() {
        signal.throwIfAborted()
        if (finished) throw new Error('The script request has finished')
    }
    async function beforePlugin() {
        requireRunning()
        if (!opened) {
            await (opening ??= open())
            requireRunning()
        }
    }
    async function run(): Promise<requestDataResponse> {
        signal.throwIfAborted()
        const response = await request(signal, beforePlugin)
        if (response.type === 'streaming' && signal.aborted) {
            void response.result.cancel(signal.reason).catch(() => {})
        }
        signal.throwIfAborted()
        if (response.type !== 'streaming') return response
        if (!collect) {
            // Legacy script calls reject a streaming response without reading it.
            void response.result.cancel().catch(() => {})
            return response
        }
        // The stream can reach its first plugin tool call after the provider returns it.
        const stream = response.result.pipeThrough(new TransformStream<StreamResponseChunk, StreamResponseChunk>(), { signal })
        return { ...response, ...await collect(stream) }
    }
    try {
        const result = await Promise.race([run(), cancellation])
        signal.throwIfAborted()
        return result
    } finally {
        finished = true
        signal.removeEventListener('abort', onAbort)
        dispose()
        if (opened) await close()
        else if (opening && !signal.aborted) await opening
    }
}
