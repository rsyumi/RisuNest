import type { ChatEditorDraft } from 'src/ts/chatEditorDrafts'

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
    viewportBindings: [] as {
        instanceId: number
        index: number
        rowKey: string
    }[],
    throwNextMount: false,
    holdDisplay: false,
    windowDialogs: false,
    pendingDisplays: new Map<number, () => void>(),
    activeEditors: new Set<number>(),
    closeEditors: new Map<number, () => void>(),
    editorDrafts: new Map<number, Omit<ChatEditorDraft, 'index' | 'caret'>>(),
    restored: [] as { instanceId: number; draft: ChatEditorDraft }[],
}

export function resetChatMountProbe() {
    chatMountProbe.nextInstanceId = 0
    chatMountProbe.mounts = []
    chatMountProbe.unmounts = []
    chatMountProbe.streamingUpdates = []
    chatMountProbe.displayUpdates = []
    chatMountProbe.viewportBindings = []
    chatMountProbe.throwNextMount = false
    chatMountProbe.holdDisplay = false
    chatMountProbe.windowDialogs = false
    chatMountProbe.pendingDisplays.clear()
    chatMountProbe.activeEditors = new Set()
    chatMountProbe.closeEditors = new Map()
    chatMountProbe.editorDrafts = new Map()
    chatMountProbe.restored = []
}
