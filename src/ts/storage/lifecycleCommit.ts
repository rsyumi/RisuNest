import { flushPendingData } from './persistentDataRuntime.svelte'
import { isTauri } from '../platform'
import { checkpointNativePersistentStore } from './nativePersistentMaintenance'

export type LifecycleCommitReason =
    | 'pagehide'
    | 'visibility-hidden'
    | 'stop'
    | 'trim-memory'
    | 'exit'

type LifecycleFlush = (reason: LifecycleCommitReason) => Promise<void>
type LifecycleCheckpoint = (mode: 'truncate') => Promise<void>

export interface LifecycleExitSyncPolicy {
    isSyncActive(): boolean
    hasPendingSync(): boolean
    confirmExit(): Promise<boolean>
}

interface NativeLifecycleDetail {
    reason?: unknown
    ackToken?: unknown
}

interface NativeLifecycleFlushBridge {
    onFlushComplete?: (token: string) => void
    onFlushHold?: (token: string) => void
    requestExit?: () => void
}

function nativeBridge(): NativeLifecycleFlushBridge | undefined {
    return (window as { RisuLifecycleBridge?: NativeLifecycleFlushBridge }).RisuLifecycleBridge
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

async function settleLifecycleCommit(
    reason: LifecycleCommitReason,
    flush: LifecycleFlush,
    checkpoint?: LifecycleCheckpoint,
): Promise<void> {
    try {
        await flush(reason)
    } catch (error) {
        console.error(`Lifecycle flush failed for ${reason}`, error)
    }

    if (!checkpoint) return

    try {
        await checkpoint('truncate')
    } catch (error) {
        console.error(`Lifecycle checkpoint failed for ${reason}`, error)
    }
}

export function registerLifecycleCommitListeners(
    flush: LifecycleFlush = flushPendingData,
    exitSyncPolicy?: LifecycleExitSyncPolicy,
    checkpoint: LifecycleCheckpoint | undefined = productionCheckpoint,
): () => void {
    const requestFlush = (reason: LifecycleCommitReason, ackToken?: string) => {
        void settleLifecycleCommit(reason, flush, checkpoint).then(() => {
            if (ackToken !== undefined) {
                acknowledgeNativeFlush(ackToken)
            }
        })
    }
    const requestExitFlush = (ackToken: string) => {
        if (!exitSyncPolicy?.isSyncActive() || !holdNativeExit(ackToken)) {
            requestFlush('exit', ackToken)
            return
        }
        void settleLifecycleCommit('exit', flush, checkpoint)
            .then(async () => {
                if (!exitSyncPolicy.hasPendingSync() || await exitSyncPolicy.confirmExit()) {
                    requestNativeExit()
                }
            })
            .catch((error) => {
                console.error('Lifecycle exit confirmation failed', error)
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
        if (reason !== 'stop' && reason !== 'trim-memory' && reason !== 'exit') return
        const ackToken = typeof detail?.ackToken === 'string' ? detail.ackToken : undefined
        if (reason === 'exit' && ackToken !== undefined) {
            requestExitFlush(ackToken)
            return
        }
        requestFlush(reason, ackToken)
    }

    window.addEventListener('pagehide', onPageHide)
    document.addEventListener('visibilitychange', onVisibilityChange)
    window.addEventListener('risu-native-lifecycle', onNativeLifecycle)

    return () => {
        window.removeEventListener('pagehide', onPageHide)
        document.removeEventListener('visibilitychange', onVisibilityChange)
        window.removeEventListener('risu-native-lifecycle', onNativeLifecycle)
    }
}
