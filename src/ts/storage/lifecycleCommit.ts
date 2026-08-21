import { flushPendingData } from './persistentDataRuntime.svelte'

export type LifecycleCommitReason =
    | 'pagehide'
    | 'visibility-hidden'
    | 'stop'
    | 'trim-memory'

type LifecycleFlush = (reason: LifecycleCommitReason) => Promise<void>

interface NativeLifecycleDetail {
    reason?: unknown
}

export function registerLifecycleCommitListeners(
    flush: LifecycleFlush = flushPendingData,
): () => void {
    const requestFlush = (reason: LifecycleCommitReason) => {
        void flush(reason).catch((error) => {
            console.error(`Lifecycle flush failed for ${reason}`, error)
        })
    }
    const onPageHide = () => requestFlush('pagehide')
    const onVisibilityChange = () => {
        if (document.visibilityState === 'hidden') {
            requestFlush('visibility-hidden')
        }
    }
    const onNativeLifecycle = (event: Event) => {
        const reason = (event as CustomEvent<NativeLifecycleDetail>).detail?.reason
        if (reason === 'stop' || reason === 'trim-memory') {
            requestFlush(reason)
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
