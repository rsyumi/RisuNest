import { flushPendingData } from './persistentDataRuntime.svelte'

export type LifecycleCommitReason =
    | 'pagehide'
    | 'visibility-hidden'
    | 'stop'
    | 'trim-memory'
    | 'exit'

type LifecycleFlush = (reason: LifecycleCommitReason) => Promise<void>

interface NativeLifecycleDetail {
    reason?: unknown
    ackToken?: unknown
}

interface NativeLifecycleFlushBridge {
    onFlushComplete?: (token: string) => void
}

function acknowledgeNativeFlush(token: string): void {
    const bridge = (window as { RisuLifecycleBridge?: NativeLifecycleFlushBridge }).RisuLifecycleBridge
    try {
        bridge?.onFlushComplete?.(token)
    } catch (error) {
        console.error('Lifecycle flush acknowledgement failed', error)
    }
}

export function registerLifecycleCommitListeners(
    flush: LifecycleFlush = flushPendingData,
): () => void {
    const requestFlush = (reason: LifecycleCommitReason, ackToken?: string) => {
        void flush(reason)
            .catch((error) => {
                console.error(`Lifecycle flush failed for ${reason}`, error)
            })
            .finally(() => {
                if (ackToken !== undefined) {
                    acknowledgeNativeFlush(ackToken)
                }
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
        if (reason === 'stop' || reason === 'trim-memory' || reason === 'exit') {
            requestFlush(reason, typeof detail?.ackToken === 'string' ? detail.ackToken : undefined)
        }
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
