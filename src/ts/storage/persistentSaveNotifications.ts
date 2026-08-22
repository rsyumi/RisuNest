import type { DataRevision } from './persistentDataStore'

export interface PersistentRuntimeNotificationCallbacks {
    onLocalRevision?: (revision: DataRevision) => void
    onFlushPromise?: (promise: Promise<void> | null) => void
    onBackgroundError?: (error: unknown) => void
}

interface PersistentSaveChannel {
    onmessage: ((event: MessageEvent) => void) | null
    postMessage(value: unknown): void
    close(): void
}

interface VisibilityDocument {
    readonly visibilityState: DocumentVisibilityState
    addEventListener(type: 'visibilitychange', listener: () => void): void
    removeEventListener(type: 'visibilitychange', listener: () => void): void
}

export interface PersistentSaveNotificationDependencies {
    sessionId: string
    channel: PersistentSaveChannel | null
    configureRuntime(callbacks: PersistentRuntimeNotificationCallbacks): void
    refreshActivePayloadRoot?(): Promise<unknown>
    visibilityDocument?: VisibilityDocument
    showForeignRevisionWarning(): void
    setSaving(saving: boolean): void
    reportError?(error: unknown): void
}

export interface PersistentSaveObserverInstallation {
    install(installer: () => () => void): void
    stop(): void
}

let productionPayloadRefresh: (() => Promise<unknown>) | undefined

export function configurePersistentSavePayloadRefresh(
    refresh: () => Promise<unknown>,
): void {
    productionPayloadRefresh = refresh
}

export function createPersistentSaveObserverInstallation(): PersistentSaveObserverInstallation {
    let dispose: (() => void) | null = null
    return {
        install(installer) {
            const current = dispose
            dispose = null
            current?.()
            dispose = installer()
        },
        stop() {
            const current = dispose
            dispose = null
            current?.()
        },
    }
}

export function installPersistentSaveNotifications(
    dependencies: PersistentSaveNotificationDependencies,
): () => void {
    let foreignRevisionSeen = false
    const refresh = () => {
        const pending = (dependencies.refreshActivePayloadRoot ?? productionPayloadRefresh)?.()
        if (pending) void pending.catch((error) => dependencies.reportError?.(error))
    }
    if (dependencies.channel) {
        dependencies.channel.onmessage = (event) => {
            if (event.data === dependencies.sessionId) return
            refresh()
            if (!foreignRevisionSeen) {
                foreignRevisionSeen = true
                dependencies.showForeignRevisionWarning()
            }
        }
    }
    const visibilityDocument = dependencies.visibilityDocument
        ?? (typeof document === 'undefined' ? undefined : document)
    const onVisibilityChange = () => {
        if (visibilityDocument?.visibilityState === 'visible') refresh()
    }
    visibilityDocument?.addEventListener('visibilitychange', onVisibilityChange)
    dependencies.configureRuntime({
        onLocalRevision: () => dependencies.channel?.postMessage(dependencies.sessionId),
        onFlushPromise: (promise) => dependencies.setSaving(promise !== null),
        onBackgroundError: (error) => dependencies.reportError?.(error),
    })
    return () => {
        dependencies.configureRuntime({
            onLocalRevision: undefined,
            onFlushPromise: undefined,
            onBackgroundError: undefined,
        })
        if (dependencies.channel) {
            dependencies.channel.onmessage = null
            dependencies.channel.close()
        }
        visibilityDocument?.removeEventListener('visibilitychange', onVisibilityChange)
    }
}
