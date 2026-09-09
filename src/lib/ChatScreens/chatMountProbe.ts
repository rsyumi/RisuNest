export interface ChatProbeMount {
    instanceId: number
    message: string
    index: number
    image: string
    character: unknown
    bookmarked?: boolean
    parserProjectionKind?: 'bounded'
    projectedChatID?: number
    parserAbortSignal?: AbortSignal
}

export interface ChatProbeStreamingUpdate {
    instanceId: number
    rawStreamingText: string
    isOptimizedStreamingMessage: boolean
}

export const chatMountProbe = {
    nextInstanceId: 0,
    mounts: [] as ChatProbeMount[],
    unmounts: [] as number[],
    streamingUpdates: [] as ChatProbeStreamingUpdate[],
    displayUpdates: [] as {
        instanceId: number
        index: number
        message: string
        signal?: AbortSignal
    }[],
    throwNextMount: false,
}

export function resetChatMountProbe() {
    chatMountProbe.nextInstanceId = 0
    chatMountProbe.mounts = []
    chatMountProbe.unmounts = []
    chatMountProbe.streamingUpdates = []
    chatMountProbe.displayUpdates = []
    chatMountProbe.throwNextMount = false
}
