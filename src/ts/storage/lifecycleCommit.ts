import { flushPendingData } from './persistentDataRuntime.svelte'
import { isTauri } from '../platform'
import { generationDiagnostics } from '../generationDiagnostics'
import { checkpointNativePersistentStore } from './nativePersistentMaintenance'
import type { SyncExitDisposition } from './syncExitCoordinator'

export type LifecycleCommitReason =
    | 'pagehide'
    | 'visibility-hidden'
    | 'stop'
    | 'trim-memory'
    | 'exit'

type LifecycleFlush = (reason: LifecycleCommitReason) => Promise<void>
type LifecycleCheckpoint = (mode: 'truncate') => Promise<void>
type ConfirmExitWithoutSaving = () => Promise<boolean>
type LifecycleSettleTimeout = (
    settlement: Promise<boolean>,
    timeoutMillis: number,
) => Promise<boolean>

const EXIT_SETTLE_TIMEOUT_MILLIS = 1_500

export interface LifecycleExitSyncPolicy {
    isSyncActive(): boolean
    hasPendingSync(): boolean
    confirmExit(): Promise<boolean>
}

export interface LifecycleExitCoordinator {
    requestExit(): Promise<SyncExitDisposition>
}

type LifecycleExitHandler = LifecycleExitSyncPolicy | LifecycleExitCoordinator

function isExitCoordinator(
    handler: LifecycleExitHandler | undefined,
): handler is LifecycleExitCoordinator {
    return typeof (handler as LifecycleExitCoordinator | undefined)?.requestExit === 'function'
}

interface NativeLifecycleDetail {
    reason?: unknown
    ackToken?: unknown
    trimLevel?: unknown
}

interface NativeLifecycleFlushBridge {
    onFlushComplete?: (token: string) => void
    onFlushHold?: (token: string) => void
    exitListenerReady?: (ready: boolean) => void
    requestExit?: () => void
}

function nativeBridge(): NativeLifecycleFlushBridge | undefined {
    return (window as { RisuLifecycleBridge?: NativeLifecycleFlushBridge }).RisuLifecycleBridge
}

function setNativeExitListenerReady(ready: boolean): void {
    try {
        nativeBridge()?.exitListenerReady?.(ready)
    } catch (error) {
        console.error('Lifecycle exit readiness failed', error)
    }
}

function acknowledgeNativeFlush(token: string): void {
    try {
        nativeBridge()?.onFlushComplete?.(token)
    } catch (error) {
        console.error('Lifecycle flush acknowledgement failed', error)
    }
}

function holdNativeExit(token: string): boolean {
    const bridge = nativeBridge()
    if (
        typeof bridge?.onFlushHold !== 'function'
        || typeof bridge.requestExit !== 'function'
    ) {
        return false
    }
    try {
        bridge.onFlushHold(token)
        return true
    } catch (error) {
        console.error('Lifecycle exit hold failed', error)
        return false
    }
}

function requestNativeExit(): void {
    try {
        nativeBridge()?.requestExit?.()
    } catch (error) {
        console.error('Lifecycle exit request failed', error)
    }
}

const productionCheckpoint: LifecycleCheckpoint | undefined = isTauri
    ? checkpointNativePersistentStore
    : undefined

function productionLifecycleSettleTimeout(
    settlement: Promise<boolean>,
    timeoutMillis: number,
): Promise<boolean> {
    return new Promise((resolve, reject) => {
        const timeout = globalThis.setTimeout(() => {
            reject(new Error(`Lifecycle save did not settle within ${timeoutMillis} ms`))
        }, timeoutMillis)
        settlement.then(
            (settled) => {
                globalThis.clearTimeout(timeout)
                resolve(settled)
            },
            (error) => {
                globalThis.clearTimeout(timeout)
                reject(error)
            },
        )
    })
}

async function settleLifecycleCommit(
    reason: LifecycleCommitReason,
    flush: LifecycleFlush,
    checkpoint?: LifecycleCheckpoint,
): Promise<boolean> {
    let settled = true
    try {
        await flush(reason)
    } catch (error) {
        settled = false
        console.error(`Lifecycle flush failed for ${reason}`, error)
    }

    if (!checkpoint) return settled

    try {
        await checkpoint('truncate')
    } catch (error) {
        settled = false
        console.error(`Lifecycle checkpoint failed for ${reason}`, error)
    }
    return settled
}

async function confirmDefaultExitWithoutSaving(): Promise<boolean> {
    const { alertConfirm } = await import('../alert')
    return alertConfirm(
        'Saving failed. Choose Yes to exit without saving, or No to retry saving.',
    )
}

export function registerLifecycleCommitListeners(
    flush: LifecycleFlush = flushPendingData,
    exitHandler?: LifecycleExitHandler,
    checkpoint: LifecycleCheckpoint | undefined = productionCheckpoint,
    confirmExitWithoutSaving: ConfirmExitWithoutSaving = confirmDefaultExitWithoutSaving,
    settleTimeout: LifecycleSettleTimeout = productionLifecycleSettleTimeout,
): () => void {
    let pendingExit: Promise<void> | undefined
    const requestFlush = (reason: LifecycleCommitReason, ackToken?: string) => {
        void settleLifecycleCommit(reason, flush, checkpoint).then(() => {
            if (ackToken !== undefined) {
                acknowledgeNativeFlush(ackToken)
            }
        })
    }
    const requestExitFlush = (ackToken: string) => {
        if (!holdNativeExit(ackToken)) {
            requestFlush('exit', ackToken)
            return
        }
        if (pendingExit) return
        pendingExit = (async () => {
            if (isExitCoordinator(exitHandler)) {
                if (await exitHandler.requestExit() === 'exit') requestNativeExit()
                return
            }
            let settled = false
            do {
                try {
                    settled = await settleTimeout(
                        settleLifecycleCommit('exit', flush, checkpoint),
                        EXIT_SETTLE_TIMEOUT_MILLIS,
                    )
                } catch (error) {
                    console.error('Lifecycle save timed out for exit', error)
                    settled = false
                }
                if (settled) break
                if (await confirmExitWithoutSaving()) {
                    requestNativeExit()
                    return
                }
            } while (!settled)
            if (
                exitHandler?.isSyncActive()
                && exitHandler.hasPendingSync()
                && !await exitHandler.confirmExit()
            ) {
                return
            }
            requestNativeExit()
        })()
            .catch((error) => {
                console.error('Lifecycle exit confirmation failed', error)
            })
            .finally(() => {
                pendingExit = undefined
            })
    }
    const onPageHide = () => requestFlush('pagehide')
    const onVisibilityChange = () => {
        if (document.visibilityState === 'hidden') {
            requestFlush('visibility-hidden')
        }
    }
    const onNativeLifecycle = (event: Event) => {
        const detail = (event as CustomEvent<NativeLifecycleDetail>).detail
        const reason = detail?.reason
        // Android reports the return to the screen only for the log.
        if (reason === 'start') {
            generationDiagnostics.lifecycle('start')
            return
        }
        if (reason !== 'stop' && reason !== 'trim-memory' && reason !== 'exit') return
        const level = detail?.trimLevel
        const trimLevel = reason === 'trim-memory' && typeof level === 'number' && Number.isSafeInteger(level) ? level : undefined
        generationDiagnostics.lifecycle(reason, trimLevel)
        const ackToken = typeof detail?.ackToken === 'string' ? detail.ackToken : undefined
        if (reason === 'exit' && ackToken !== undefined) {
            requestExitFlush(ackToken)
            return
        }
        requestFlush(reason, ackToken)
    }

    // Chromium freezes a hidden page and resumes it on return; Android IPC sends the freeze before the page stops.
    const onFreeze = () => generationDiagnostics.lifecycle('freeze')
    const onResume = () => generationDiagnostics.lifecycle('resume')

    window.addEventListener('pagehide', onPageHide)
    document.addEventListener('visibilitychange', onVisibilityChange)
    document.addEventListener('freeze', onFreeze)
    document.addEventListener('resume', onResume)
    window.addEventListener('risu-native-lifecycle', onNativeLifecycle)
    setNativeExitListenerReady(true)

    return () => {
        setNativeExitListenerReady(false)
        window.removeEventListener('pagehide', onPageHide)
        document.removeEventListener('visibilitychange', onVisibilityChange)
        document.removeEventListener('freeze', onFreeze)
        document.removeEventListener('resume', onResume)
        window.removeEventListener('risu-native-lifecycle', onNativeLifecycle)
    }
}
