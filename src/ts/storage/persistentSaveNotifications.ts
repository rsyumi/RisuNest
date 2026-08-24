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

export interface PersistentSaveNotificationDependencies {
    sessionId: string
    channel: PersistentSaveChannel | null
    configureRuntime(callbacks: PersistentRuntimeNotificationCallbacks): void
    showForeignRevisionWarning(): void
    setSaving(saving: boolean): void
    reportError?(error: unknown): void
    onSaveCommitted?(): void
}

export interface PersistentSaveObserverInstallation {
    install(installer: () => () => void): void
    stop(): void
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
    if (dependencies.channel) {
        dependencies.channel.onmessage = (event) => {
            if (event.data === dependencies.sessionId) return
            if (!foreignRevisionSeen) {
                foreignRevisionSeen = true
                dependencies.showForeignRevisionWarning()
            }
        }
    }
    dependencies.configureRuntime({
        onLocalRevision: () => {
            dependencies.channel?.postMessage(dependencies.sessionId)
            dependencies.onSaveCommitted?.()
        },
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
    }
}
