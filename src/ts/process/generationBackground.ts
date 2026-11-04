import { isTauriAndroid, isTauriIOS } from '../platform'
import { getDeviceSettings } from '../storage/deviceSettings'
import { beginDeferredMobileBackgroundTask, beginMobileBackgroundTask, retainMobileBackgroundActivity, type MobileBackgroundTask } from '../mobileBackgroundTask'
import { beginIOSGeneration, isBackgroundExpiryReason } from '../iosNative'
import { isGenerationReserved, joinGenerationSignal, type GenerationBackground } from './generationState'

// On Android the keep-alive setting decides whether generation holds the service.
const androidKeepAlive = () => getDeviceSettings().androidKeepAliveDuringGeneration

/** The Android keep-alive and the iOS generation task for one reserved generation. */
export function beginGenerationBackground(): GenerationBackground {
    const controller = new AbortController()
    const forwardExpiry = <T extends { signal?: AbortSignal }>(task: T): T => {
        const signal = task.signal
        const abort = () => { if (isBackgroundExpiryReason(signal?.reason)) controller.abort(signal!.reason) }
        if (signal?.aborted) abort()
        else signal?.addEventListener('abort', abort, { once: true })
        return task
    }
    const android: Promise<MobileBackgroundTask> | null = isTauriAndroid && androidKeepAlive()
        ? Promise.resolve(beginMobileBackgroundTask('generation')).then(forwardExpiry) : null
    const releaseIOSActivity = isTauriIOS ? retainMobileBackgroundActivity() : undefined
    const ios = isTauriIOS ? beginIOSGeneration().then(task => {
        forwardExpiry(task)
        if (!task.admitted() || task.signal?.aborted) releaseIOSActivity?.()
        else task.signal?.addEventListener('abort', () => releaseIOSActivity?.(), { once: true })
        return task
    }, error => { releaseIOSActivity?.(); throw error }) : null
    let succeeded = false
    let expiryTaken = false
    let ended = false
    return {
        signal: controller.signal,
        progress(completed) { void ios?.then(task => task.progress(completed)) },
        settle(success) { succeeded = success },
        takeExpiry() {
            if (expiryTaken || !isBackgroundExpiryReason(controller.signal.reason)) return false
            expiryTaken = true
            return true
        },
        async end() {
            if (ended) return
            ended = true
            await Promise.all([
                android?.then(task => task.dispose(succeeded)),
                ios?.then(task => task.dispose(succeeded && !controller.signal.aborted)),
            ]).catch(error => console.error('Generation background cleanup failed', error)).finally(releaseIOSActivity)
        },
    }
}

function releaseWhenRead<R>(body: ReadableStream<R>, release: () => void): ReadableStream<R> {
    const reader = body.getReader()
    return new ReadableStream<R>({
        async pull(controller) {
            try {
                const { done, value } = await reader.read()
                if (done) {
                    release()
                    controller.close()
                } else controller.enqueue(value)
            } catch (error) {
                release()
                controller.error(error)
            }
        },
        cancel(reason) {
            release()
            return reader.cancel(reason)
        },
    })
}

/**
 * Keeps a model call outside a reserved generation running after Home without a notification while visible.
 * A streamed response holds until its body is read to the end, cancelled, or the call is aborted.
 */
export async function holdModelCall<T>(operation: (signal?: AbortSignal) => Promise<T>, signal?: AbortSignal | null): Promise<T> {
    const caller = signal ?? undefined
    // A reserved generation already holds the platform's protection for its own calls.
    if ((!isTauriAndroid && !isTauriIOS) || (isTauriAndroid && !androidKeepAlive()) || isGenerationReserved()) return operation(caller)
    // Native admission runs beside the call so a busy bridge never delays the request.
    const hold = Promise.resolve(beginDeferredMobileBackgroundTask('generation'))
    // When the platform's time runs out, the call stops as it would for the caller's cancel.
    const expiry = new AbortController()
    void hold.then(task => {
        const held = task.signal
        if (held?.aborted) expiry.abort(held.reason)
        else held?.addEventListener('abort', () => expiry.abort(held.reason), { once: true })
    })
    const joined = joinGenerationSignal(caller, expiry.signal)
    let released = false
    const release = () => {
        if (released) return
        released = true
        caller?.removeEventListener('abort', release)
        joined.release()
        void hold.then(task => task.dispose())
    }
    caller?.addEventListener('abort', release, { once: true })
    let streaming = false
    try {
        const value = await operation(joined.signal)
        const response = value as { type?: unknown; result?: unknown } | null
        if (response?.type === 'streaming' && response.result instanceof ReadableStream && !joined.signal?.aborted) {
            streaming = true
            return { ...response, result: releaseWhenRead(response.result, release) } as T
        }
        return value
    } finally {
        if (!streaming) release()
    }
}

/**
 * Keeps bot script work that a chat button, form or manual trigger starts running after Home, as a model call is kept.
 * Its model calls still take their own holds, so each one stops when the platform's time runs out.
 */
export async function holdScriptRun<T>(operation: () => Promise<T>): Promise<T> {
    // A reserved generation can end before the script does, so the script holds even inside one.
    if ((!isTauriAndroid && !isTauriIOS) || (isTauriAndroid && !androidKeepAlive())) return operation()
    const hold = Promise.resolve(beginDeferredMobileBackgroundTask('generation'))
    try {
        return await operation()
    } finally {
        void hold.then(task => task.dispose())
    }
}
