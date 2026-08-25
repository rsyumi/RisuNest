export interface ChatProbeMount {
    instanceId: number
    message: string
    index: number
    image: string
    character: unknown
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
}

export function resetChatMountProbe() {
    chatMountProbe.nextInstanceId = 0
    chatMountProbe.mounts = []
    chatMountProbe.unmounts = []
    chatMountProbe.streamingUpdates = []
}
