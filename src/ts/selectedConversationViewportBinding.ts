import type {
    ConversationViewportSnapshot,
    ConversationViewportSource,
} from './conversationViewportSource'
import type { Message } from './storage/database.svelte'

export interface SelectedConversationViewportRuntime {
    getActiveConversationViewportSource(): ConversationViewportSource | null
    subscribeActiveConversationViewportSource(
        listener: (source: ConversationViewportSource | null) => void,
    ): () => void
}

export class SelectedConversationViewportBinding {
    private activeSource: ConversationViewportSource | null
    private activeSnapshot: ConversationViewportSnapshot | null
    private readonly runtimeUnsubscribe: () => void
    private sourceUnsubscribe: (() => void) | null = null
    private boundaryController: AbortController | null = null
    private boundaryRequestKey: string | null = null
    private firstBoundaryMessage: Readonly<Message> | undefined
    private tailBoundaryMessage: Readonly<Message> | undefined
    private disposed = false

    constructor(
        runtime: SelectedConversationViewportRuntime,
        private readonly onChange: () => void,
    ) {
        this.activeSource = runtime.getActiveConversationViewportSource()
        this.activeSnapshot = this.activeSource?.snapshot() ?? null
        this.captureBoundaryMessages(true)
        this.subscribeToActiveSource()
        this.runtimeUnsubscribe = runtime.subscribeActiveConversationViewportSource((source) => {
            this.replaceSource(source)
        })
        this.requestBoundaryRows()
    }

    get source(): ConversationViewportSource | null {
        return this.activeSource
    }

    get totalMessages(): number {
        return this.activeSnapshot?.totalMessages ?? 0
    }

    get firstMessage(): Readonly<Message> | undefined {
        return this.firstBoundaryMessage
    }

    get tailMessage(): Readonly<Message> | undefined {
        return this.tailBoundaryMessage
    }

    dispose(): void {
        if (this.disposed) return
        this.disposed = true
        this.runtimeUnsubscribe()
        this.sourceUnsubscribe?.()
        this.sourceUnsubscribe = null
        this.cancelBoundaryRequest()
        this.activeSource = null
        this.activeSnapshot = null
        this.firstBoundaryMessage = undefined
        this.tailBoundaryMessage = undefined
    }

    private replaceSource(source: ConversationViewportSource | null): void {
        if (this.disposed) return
        if (source !== this.activeSource) {
            this.sourceUnsubscribe?.()
            this.sourceUnsubscribe = null
            this.cancelBoundaryRequest()
            this.activeSource = source
            this.subscribeToActiveSource()
        }
        const nextSnapshot = source?.snapshot() ?? null
        const resetBoundaries = (
            this.activeSnapshot?.sourceToken !== nextSnapshot?.sourceToken ||
            this.activeSnapshot?.version !== nextSnapshot?.version ||
            this.activeSnapshot?.totalMessages !== nextSnapshot?.totalMessages
        )
        this.activeSnapshot = nextSnapshot
        this.captureBoundaryMessages(resetBoundaries)
        this.onChange()
        this.requestBoundaryRows()
    }

    private subscribeToActiveSource(): void {
        const source = this.activeSource
        if (!source) return
        this.sourceUnsubscribe = source.subscribe(() => {
            if (this.disposed || source !== this.activeSource) return
            const previousSnapshot = this.activeSnapshot
            this.activeSnapshot = source.snapshot()
            const resetBoundaries = (
                previousSnapshot?.sourceToken !== this.activeSnapshot.sourceToken ||
                previousSnapshot?.version !== this.activeSnapshot.version ||
                previousSnapshot?.totalMessages !== this.activeSnapshot.totalMessages
            )
            if (resetBoundaries) this.cancelBoundaryRequest()
            this.captureBoundaryMessages(resetBoundaries)
            this.onChange()
            this.requestBoundaryRows()
        })
    }

    private requestBoundaryRows(): void {
        const source = this.activeSource
        const snapshot = this.activeSnapshot
        if (!source || !snapshot || snapshot.totalMessages === 0) {
            this.cancelBoundaryRequest()
            return
        }
        const missingIndices = this.firstBoundaryMessage === undefined
            ? [0]
            : []
        const tailIndex = snapshot.totalMessages - 1
        if (tailIndex !== 0 && this.tailBoundaryMessage === undefined) {
            missingIndices.push(tailIndex)
        }
        if (missingIndices.length === 0) {
            this.cancelBoundaryRequest()
            return
        }
        const requestIdentity = `${snapshot.sourceToken}:${snapshot.version}`
        if (
            this.boundaryController &&
            this.boundaryRequestKey?.startsWith(`${requestIdentity}:`)
        ) return
        const requestKey = [requestIdentity, ...missingIndices].join(':')
        if (requestKey === this.boundaryRequestKey) return
        this.cancelBoundaryRequest()
        const controller = new AbortController()
        this.boundaryController = controller
        this.boundaryRequestKey = requestKey
        void Promise.all(missingIndices.map((startIndex) => source.ensureRange({
            startIndex,
            limit: 1,
            reason: 'viewport',
            signal: controller.signal,
        }))).catch(() => undefined).finally(() => {
            if (this.boundaryController !== controller) return
            this.boundaryController = null
            this.boundaryRequestKey = null
        })
    }

    private cancelBoundaryRequest(): void {
        this.boundaryController?.abort()
        this.boundaryController = null
        this.boundaryRequestKey = null
    }

    private captureBoundaryMessages(reset: boolean): void {
        if (reset) {
            this.firstBoundaryMessage = undefined
            this.tailBoundaryMessage = undefined
        }
        const snapshot = this.activeSnapshot
        if (!snapshot || snapshot.totalMessages === 0) {
            this.firstBoundaryMessage = undefined
            this.tailBoundaryMessage = undefined
            return
        }
        this.firstBoundaryMessage ??= snapshot.rowAt(0)?.message
        this.tailBoundaryMessage ??= snapshot.rowAt(snapshot.totalMessages - 1)?.message
    }
}
