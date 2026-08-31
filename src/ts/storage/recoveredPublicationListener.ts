import { type AndroidSafDestinationEvent } from './androidSafBridge'

type RecoveredTerminal = Promise<AndroidSafDestinationEvent | null> | AndroidSafDestinationEvent | null

export interface RecoveredPublicationListenerOptions {
    sourceKind: AndroidSafDestinationEvent['sourceKind']
    onTerminal(terminal: AndroidSafDestinationEvent): void
    onError(error: unknown): void
    listen(listener: (event: AndroidSafDestinationEvent) => void): () => void
    isActive(requestId: string): boolean
    recoverEvent(event: AndroidSafDestinationEvent): RecoveredTerminal
    /** Recovery of a terminal already present at listen time, if any. */
    initial: { requestId?: string; recover(): RecoveredTerminal } | null
}

/**
 * Shared skeleton for the Android SAF recovered-publication listeners: a
 * serial queue that recovers each terminal exactly once (by request id),
 * keeps running after individual recovery failures, and replays a terminal
 * that was already persisted before the listener attached.
 */
export function listenRecoveredPublications(
    options: RecoveredPublicationListenerOptions,
): () => void {
    let disposed = false
    let queue = Promise.resolve()
    const handledRequestIds = new Set<string>()
    const enqueue = (recover: () => RecoveredTerminal, requestId?: string) => {
        queue = queue.then(async () => {
            if (disposed || (requestId && handledRequestIds.has(requestId))) return
            const terminal = await recover()
            if (!terminal || handledRequestIds.has(terminal.requestId)) return
            handledRequestIds.add(terminal.requestId)
            options.onTerminal(terminal)
        }).catch(options.onError)
    }
    const disposeListener = options.listen((event) => {
        if (event.sourceKind !== options.sourceKind || options.isActive(event.requestId)) return
        enqueue(() => options.recoverEvent(event), event.requestId)
    })
    if (options.initial) {
        const { requestId, recover } = options.initial
        enqueue(recover, requestId)
    }
    return () => {
        disposed = true
        disposeListener()
    }
}
