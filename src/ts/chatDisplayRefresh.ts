import type { CapturedChatMessageTarget } from './chatMessageUi'
import type { ConversationViewportRow } from './conversationViewportSource'
import type { BoundedLiveChatParserProjection } from './selectedConversationLiveParserProjection'

/** Published only after the current row's parser context is ready. */
export interface ChatDisplayRefresh {
    message: string
    totalMessages: number
    parserProjection?: BoundedLiveChatParserProjection
    parserAbortSignal?: AbortSignal
    viewportBinding?: {
        viewportRow: ConversationViewportRow
        viewportSourceToken: string
        captureViewportTarget: () => CapturedChatMessageTarget | null
    }
}
