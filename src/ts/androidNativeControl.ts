import type { AndroidGenerationKeepAliveBridge } from './androidGenerationKeepAlive'
import type { AndroidSafJavascriptBridge } from './storage/androidSafBridge'

export interface AndroidControlMessagePort {
    postMessage(message: string): void
    onmessage?: ((event: { data: string }) => void) | null
}

interface AndroidLifecycleControl {
    onFlushComplete?(token: string): void
    onFlushHold?(token: string): void
    requestExit?(): void
    requestRestart?(): void
}

declare global {
    interface Window {
        RisuNestControl?: AndroidControlMessagePort
        RisuNestSafControl?: AndroidControlMessagePort
        RisuLifecycleBridge?: AndroidLifecycleControl
        RisuSafBridge?: AndroidSafJavascriptBridge
    }
}

export function createAndroidControlClient(port: AndroidControlMessagePort) {
    const pending = new Map<string, {
        resolve(value: unknown): void
        reject(error: Error): void
        timer: ReturnType<typeof setTimeout>
    }>()
    port.onmessage = (event) => {
        let response: { id?: unknown; result?: unknown; error?: unknown }
        try {
            response = JSON.parse(event.data)
        } catch {
            return
        }
        if (!response || typeof response.id !== 'string') return
        const request = pending.get(response.id)
        if (!request) return
        pending.delete(response.id)
        clearTimeout(request.timer)
        if (typeof response.error === 'string') request.reject(new Error(response.error))
        else request.resolve(response.result)
    }
    return {
        notify(method: string, ...args: string[]): void {
            port.postMessage(JSON.stringify({ method, args }))
        },
        request<T>(method: string, ...args: string[]): Promise<T> {
            if (pending.size >= 32) return Promise.reject(new Error('android-control-busy'))
            const id = crypto.randomUUID()
            return new Promise<T>((resolve, reject) => {
                const timer = setTimeout(() => {
                    pending.delete(id)
                    reject(new Error('android-control-timeout'))
                }, 15_000)
                pending.set(id, { resolve: (value) => resolve(value as T), reject, timer })
                try {
                    port.postMessage(JSON.stringify({ id, method, args }))
                } catch (error) {
                    pending.delete(id)
                    clearTimeout(timer)
                    reject(error)
                }
            })
        },
    }
}

function installAndroidNativeControl(): void {
    if (typeof window === 'undefined' || !window.RisuNestControl) return
    const control = createAndroidControlClient(window.RisuNestControl)
    window.RisuLifecycleBridge = {
        onFlushComplete: (token) => control.notify('lifecycle.onFlushComplete', token),
        onFlushHold: (token) => control.notify('lifecycle.onFlushHold', token),
        requestExit: () => control.notify('lifecycle.requestExit'),
        requestRestart: () => control.notify('lifecycle.requestRestart'),
    }
    const generation: AndroidGenerationKeepAliveBridge = {
        begin: () => control.request<boolean>('generation.begin'),
        end: () => control.request<boolean>('generation.end'),
        notificationsEnabled: () => control.request<boolean>('generation.notificationsEnabled'),
        requestNotifications: () => control.request<void>('generation.requestNotifications'),
        openNotificationSettings: () => control.request<boolean>('generation.openNotificationSettings'),
        webViewVersion: () => control.request<string>('generation.webViewVersion'),
    }
    window.RisuGenerationKeepAlive = generation
    if (!window.RisuNestSafControl) {
        control.notify('lifecycle.onFrontendReady')
        return
    }
    const saf = createAndroidControlClient(window.RisuNestSafControl)
    window.RisuSafBridge = {
        copyExport: (...args) => saf.notify('saf.copyExport', ...args),
        cancelExport: (id) => saf.request<boolean>('saf.cancelExport', id),
        cancelSource: (id) => saf.notify('saf.cancelSource', id),
        pickBackupSource: (id) => saf.notify('saf.pickBackupSource', id),
        pickContentSource: (id, destination) => saf.notify('saf.pickContentSource', id, destination),
        pickLegacyBackupSource: (id) => saf.notify('saf.pickLegacyBackupSource', id),
        discardSource: (token) => saf.request<boolean>('saf.discardSource', token),
        getActiveSourceRequestIds: () => saf.request<string>('saf.getActiveSourceRequestIds'),
        getExportStatus: () => saf.request<string | null>('saf.getExportStatus'),
        getExportSourceId: () => saf.request<string | null>('saf.getExportSourceId'),
        markExportPublicationReady: (id) => saf.request<boolean>('saf.markExportPublicationReady', id),
        acknowledgeExport: (id) => saf.request<boolean>('saf.acknowledgeExport', id),
    }
    control.notify('lifecycle.onFrontendReady')
}

installAndroidNativeControl()
