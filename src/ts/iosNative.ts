import { invoke } from '@tauri-apps/api/core'
import { isTauriIOS } from './platform'

export interface IOSNativeState {
    notifications: boolean
    notificationStatus: number
    activeTasks: string[]
    expiredTasks: string[]
    foreground: boolean
    backgroundMode: 'limited' | 'continued'
}

export const getIOSNativeState = () =>
    invoke<IOSNativeState>('plugin:ios-native|state')
export const requestIOSNotifications = () =>
    invoke<{ granted: boolean }>('plugin:ios-native|request_notifications')
export const openIOSSettings = () =>
    invoke<{ opened: boolean }>('plugin:ios-native|open_settings')
export const notifyIOSGenerationComplete = () =>
    invoke('plugin:ios-native|notify', {
        body: navigator.language.startsWith('ko')
            ? '응답 생성이 완료되었습니다.'
            : 'Your response is ready.',
    })

interface IOSGenerationDependencies {
    enabled(): boolean
    begin(): Promise<{ id: string | null }>
    end(id: string): Promise<unknown>
    state(): Promise<IOSNativeState>
    events: EventTarget
}
const productionDependencies: IOSGenerationDependencies = {
    enabled: () => isTauriIOS,
    begin: () => invoke('plugin:ios-native|begin'),
    end: (id) => invoke('plugin:ios-native|end', { id }),
    state: getIOSNativeState,
    events: window,
}

/** Retain the caller's cancellation and pair every native assertion with release. */
export async function beginIOSGeneration(
    signal?: AbortSignal,
    deps: IOSGenerationDependencies = productionDependencies,
): Promise<{ signal: AbortSignal | undefined; dispose(): Promise<void> }> {
    if (!deps.enabled()) return { signal, dispose: async () => {} }
    const controller = new AbortController()
    const abort = () => controller.abort(signal?.reason)
    signal?.addEventListener('abort', abort, { once: true })
    if (signal?.aborted) abort()
    let id: string | null = null
    const expired = () =>
        controller.abort(
            new DOMException('iOS background execution expired', 'AbortError'),
        )
    const listener = (event: Event) => {
        const detail = (event as CustomEvent<{ event: string; id?: string }>)
            .detail
        if (detail?.event === 'expired' && detail.id === id) expired()
        if (detail?.event === 'active' && id) {
            // WebKit can suspend before delivery of the expiration event.
            void deps
                .state()
                .then((state) => {
                    if (
                        id &&
                        (!state.activeTasks.includes(id) ||
                            state.expiredTasks.includes(id))
                    )
                        expired()
                })
                .catch(() => expired())
        }
    }
    deps.events.addEventListener('risunest-ios-lifecycle', listener)
    try {
        id = (await deps.begin()).id
    } catch {
        /* Foreground generation remains available when iOS rejects extra runtime. */
    }
    let disposed = false
    return {
        signal: controller.signal,
        async dispose() {
            if (disposed) return
            disposed = true
            deps.events.removeEventListener('risunest-ios-lifecycle', listener)
            signal?.removeEventListener('abort', abort)
            if (id) await deps.end(id)
        },
    }
}

export function installIOSPersistenceLifecycle(
    flush: (reason: string) => Promise<void>,
): () => void {
    if (!isTauriIOS) return () => {}
    let pending = Promise.resolve()
    const save = (id?: string) => {
        pending = pending
            .then(() => flush('ios-lifecycle'))
            .catch((error) => {
                console.error('iOS lifecycle save failed', error)
            })
            .finally(async () => {
                if (id) await invoke('plugin:ios-native|end', { id }).catch(() => {})
            })
    }
    const visibility = () => {
        if (document.hidden) save()
    }
    const native = (event: Event) => {
        const detail = (event as CustomEvent<{ event: string; id?: string }>).detail
        if (detail?.event === 'background') save(detail.id)
        else if (detail?.event === 'expired') save()
    }
    document.addEventListener('visibilitychange', visibility)
    window.addEventListener('risunest-ios-lifecycle', native)
    return () => {
        document.removeEventListener('visibilitychange', visibility)
        window.removeEventListener('risunest-ios-lifecycle', native)
    }
}
