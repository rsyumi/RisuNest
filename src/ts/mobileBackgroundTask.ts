import './androidNativeControl'
import { isTauriAndroid, isTauriIOS } from './platform'
import { beginIOSBackgroundTask, BackgroundExecutionExpiredError, isBackgroundExpiryReason, isIOSTaskBegun } from './iosNative'
import { measuredTaskPercent } from './taskPercent'

export { measuredTaskPercent }

export type MobileTaskKind = 'backup' | 'restore' | 'sync' | 'import' | 'export' | 'maintenance' | 'generation'
export type MobileTaskRefusal = 'background' | 'unavailable' | 'timeout' | 'failed'
export type MobileTaskProtection =
    | { state: 'active' }
    | { state: 'deferred' }
    | { state: 'refused'; reason: MobileTaskRefusal }
export interface MobileBackgroundTask {
    signal?: AbortSignal
    protection?: MobileTaskProtection
    expired?(): boolean
    progress(percent: number | null): void
    dispose(success?: boolean): Promise<void>
}
/** Native replies `active`, `deferred`, or the reason it refused; a refused hold stays registered until it ends. */
export interface AndroidBackgroundTaskBridge {
    begin(id: string, kind: MobileTaskKind, deferred: boolean, userInitiated: boolean): Promise<string>
    progress(id: string, percent: number): Promise<unknown>
    end(id: string): Promise<unknown>
}
declare global {
    interface Window { RisuBackgroundTasks?: AndroidBackgroundTaskBridge }
}
const active = new Set<object>()
const listeners = new Set<() => void>()
export const hasMobileBackgroundTasks = () => active.size > 0
export function subscribeMobileBackgroundTasks(listener: () => void): () => void {
    listeners.add(listener)
    return () => listeners.delete(listener)
}
function publish() { for (const listener of listeners) listener() }

/** Register work whose platform assertion is managed by a specialized adapter. */
export function retainMobileBackgroundActivity(): () => void {
    if (!isTauriAndroid && !isTauriIOS) return () => {}
    const owner = {}
    active.add(owner)
    publish()
    return () => { if (active.delete(owner)) publish() }
}

interface IOSDeferredHold { controller: AbortController; settled: Promise<void> }
const iosDeferredHolds = new Set<IOSDeferredHold>()

/** Resolves once no deferred hold remains, including holds that begin while waiting. */
export async function mobileDeferredHoldsSettled(): Promise<void> {
    while (iosDeferredHolds.size > 0) await Promise.all([...iosDeferredHolds].map(hold => hold.settled))
}

let iosEntryExpiryHeard = false
/**
 * Deferred holds run on the assertion iOS takes when the app leaves the screen, so its expiry ends them, also when the
 * page missed that departure during startup. Once the page heard a departure, only that departure's assertion counts.
 * A task the page began ends only its own work, and an expiry heard back on screen arrived late.
 */
function hearIOSEntryExpiry(): void {
    if (iosEntryExpiryHeard) return
    iosEntryExpiryHeard = true
    let entry: string | undefined
    window.addEventListener('risunest-ios-lifecycle', event => {
        const detail = (event as CustomEvent<{ event?: string; id?: string }>).detail
        if (detail?.event === 'background') entry = detail.id ?? ''
        if (detail?.event !== 'expired' || !detail.id || entry !== undefined && detail.id !== entry
            || isIOSTaskBegun(detail.id) || document.visibilityState !== 'hidden') return
        for (const hold of iosDeferredHolds) hold.controller.abort(new BackgroundExecutionExpiredError())
    })
}

function noticeRefusedProtection(): void {
    void Promise.all([import('./alert'), import('../lang')])
        .then(([{ alertToast }, { language }]) => alertToast(language.risuNest.backgroundWorkMayStop))
        .catch(() => {})
}

function refusalReason(status: string): MobileTaskRefusal {
    return status === 'background' || status === 'timeout' ? status : 'failed'
}

function beginTask(
    kind: MobileTaskKind,
    signal: AbortSignal | undefined,
    userInitiated: boolean,
    deferred: boolean,
): MobileBackgroundTask | Promise<MobileBackgroundTask> {
    const unprotected = (reason: MobileTaskRefusal): MobileBackgroundTask => ({
        signal, protection: { state: 'refused', reason }, progress() {}, async dispose() {},
    })
    if ((!isTauriAndroid && !isTauriIOS) || signal?.aborted) return unprotected('unavailable')
    // Register before awaiting native admission so Home cannot cancel work during acquisition.
    const release = retainMobileBackgroundActivity()
    const settle = (task: MobileBackgroundTask): MobileBackgroundTask => {
        if (userInitiated && task.protection?.state === 'refused' && task.protection.reason !== 'unavailable') {
            noticeRefusedProtection()
        }
        let disposed = false
        return {
            signal: task.signal,
            protection: task.protection,
            expired: () => task.expired?.() ?? isBackgroundExpiryReason(task.signal?.reason),
            progress: percent => task.progress(percent === null ? null : measuredTaskPercent(percent, 100)),
            async dispose(success = false) {
                if (disposed) return
                disposed = true
                try { await task.dispose(success) } catch {} finally { release() }
            },
        }
    }
    if (isTauriIOS && deferred) {
        // iOS holds the background-entry assertion until deferred holds settle.
        hearIOSEntryExpiry()
        const controller = new AbortController()
        const abort = () => controller.abort(signal?.reason)
        signal?.addEventListener('abort', abort, { once: true })
        let finish!: () => void
        const hold: IOSDeferredHold = { controller, settled: new Promise<void>(resolve => { finish = resolve }) }
        iosDeferredHolds.add(hold)
        return settle({
            signal: controller.signal,
            protection: { state: 'deferred' },
            progress() {},
            async dispose() {
                signal?.removeEventListener('abort', abort)
                iosDeferredHolds.delete(hold)
                finish()
            },
        })
    }
    return (async () => {
        let task: MobileBackgroundTask
        try {
            if (isTauriIOS) {
                task = await beginIOSBackgroundTask(kind, signal, release, userInitiated)
            } else {
                const bridge = window.RisuBackgroundTasks
                if (!bridge) { release(); return unprotected('unavailable') }
                const id = crypto.randomUUID()
                const controller = new AbortController()
                const abort = () => controller.abort(signal?.reason)
                signal?.addEventListener('abort', abort, { once: true })
                const expired = (event: Event) => {
                    if ((event as CustomEvent<string>).detail !== id) return
                    controller.abort(new BackgroundExecutionExpiredError())
                    release()
                }
                window.addEventListener('risunest-background-expired', expired)
                let status: string
                try {
                    status = await bridge.begin(id, kind, deferred, userInitiated)
                } catch (error) {
                    // Native may still admit a request that timed out, so the hold always ends with the work.
                    status = error instanceof Error && error.message === 'android-control-timeout' ? 'timeout' : 'failed'
                }
                let pending = Promise.resolve()
                let reported: number | null | undefined
                let disposed = false
                task = {
                    signal: controller.signal,
                    protection: status === 'active' || status === 'deferred'
                        ? { state: status } : { state: 'refused', reason: refusalReason(status) },
                    progress(percent) {
                        if (disposed || controller.signal.aborted || percent === reported) return
                        reported = percent
                        pending = pending.then(async () => { await bridge.progress(id, percent ?? -1) }).catch(() => {})
                    },
                    async dispose() {
                        if (disposed) return
                        disposed = true
                        window.removeEventListener('risunest-background-expired', expired)
                        signal?.removeEventListener('abort', abort)
                        await pending
                        // Native keeps the hold, and its notification, until it hears the end, so a full channel is asked again.
                        for (let attempt = 1; ; attempt += 1) {
                            try {
                                await bridge.end(id)
                                return
                            } catch (error) {
                                if (attempt >= 5 || !(error instanceof Error) || error.message !== 'android-control-busy') throw error
                            }
                            await new Promise(resolve => setTimeout(resolve, attempt * 250))
                        }
                    },
                }
            }
        } catch { release(); return unprotected('failed') }
        return settle(task)
    })()
}

/** Protects user-started or long work at once, with a notification on Android. */
export function beginMobileBackgroundTask(kind: MobileTaskKind, signal?: AbortSignal, userInitiated = false): MobileBackgroundTask | Promise<MobileBackgroundTask> {
    return beginTask(kind, signal, userInitiated, false)
}

/** Protects routine work only if the app leaves the screen while it runs. */
export function beginDeferredMobileBackgroundTask(kind: MobileTaskKind, signal?: AbortSignal): MobileBackgroundTask | Promise<MobileBackgroundTask> {
    return beginTask(kind, signal, false, true)
}

async function runWithTask<T>(
    begin: MobileBackgroundTask | Promise<MobileBackgroundTask>,
    operation: (task: MobileBackgroundTask) => Promise<T>,
    signal?: AbortSignal,
): Promise<T> {
    const task = await begin
    let success = false
    try {
        task.signal?.throwIfAborted()
        const result = await operation(task)
        success = !task.signal?.aborted && result !== null
        return result
    } catch (error) {
        throw heldOperationError(error, task, signal)
    } finally { await task.dispose(success) }
}

function heldOperationError(error: unknown, task: MobileBackgroundTask | undefined, signal?: AbortSignal): unknown {
    if (signal?.aborted && isBackgroundExpiryReason(error)) {
        return new DOMException('Operation cancelled', 'AbortError')
    }
    if (error instanceof Error && error.name === 'AbortError' && task?.expired?.() && !signal?.aborted) {
        return new BackgroundExecutionExpiredError()
    }
    return error
}

export function runWithMobileBackgroundTask<T>(
    kind: MobileTaskKind,
    operation: (task: MobileBackgroundTask) => Promise<T>,
    signal?: AbortSignal,
    userInitiated = false,
): Promise<T> {
    if (!isTauriAndroid && !isTauriIOS) return operation({ signal, progress() {}, async dispose() {} })
    return runWithTask(beginMobileBackgroundTask(kind, signal, userInitiated), operation, signal)
}

export function runWithDeferredMobileBackgroundTask<T>(
    kind: MobileTaskKind,
    operation: (task: MobileBackgroundTask) => Promise<T>,
    signal?: AbortSignal,
): Promise<T> {
    if (!isTauriAndroid && !isTauriIOS) return operation({ signal, progress() {}, async dispose() {} })
    return runWithTask(beginDeferredMobileBackgroundTask(kind, signal), operation, signal)
}

export interface MobileBackgroundTaskAfterPicker extends MobileBackgroundTask {
    /** Begins the hold once the picker has returned; later calls do nothing. */
    protect(): void
}

/**
 * Runs user-started work that opens a system picker first, so the picker never holds a task or shows the
 * Android notification. The work's signal also ends when the hold it began expires.
 */
export async function runWithMobileBackgroundTaskAfterPicker<T>(
    kind: MobileTaskKind,
    operation: (task: MobileBackgroundTaskAfterPicker) => Promise<T>,
    signal?: AbortSignal,
): Promise<T> {
    if (!isTauriAndroid && !isTauriIOS) return operation({ signal, progress() {}, async dispose() {}, protect() {} })
    const controller = new AbortController()
    const follow = (source: AbortSignal | undefined) => {
        if (source?.aborted) controller.abort(source.reason)
        else source?.addEventListener('abort', () => controller.abort(source.reason), { once: true })
    }
    follow(signal)
    let task: MobileBackgroundTask | undefined
    let begun: Promise<void> | undefined
    let reported: number | null | undefined
    let success = false
    try {
        const result = await operation({
            signal: controller.signal,
            progress: percent => {
                reported = percent
                task?.progress(percent)
            },
            async dispose() {},
            protect() {
                begun ??= Promise.resolve(beginMobileBackgroundTask(kind, signal, true)).then(held => {
                    task = held
                    follow(held.signal)
                    // Progress reported while native admitted the hold reaches it now.
                    if (reported !== undefined) held.progress(reported)
                })
            },
        })
        success = !controller.signal.aborted && result !== null
        return result
    } catch (error) {
        await begun
        throw heldOperationError(error, task, signal)
    } finally {
        await begun
        await task?.dispose(success)
    }
}
