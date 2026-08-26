import type { ConversationMutationTarget } from '../../ts/conversationMutations'
import {
    moveConversationRerollHistory,
    replaceConversationRerollLastData,
    type ConversationRerollHistory,
} from '../../ts/conversationReroll'

interface DefaultChatUnrerollOptions {
    target: ConversationMutationTarget
    history: ConversationRerollHistory | null
    preUnreroll(generationId: string): string | null | undefined
}

export type DefaultChatUnrerollResult =
    | { type: 'precomputed' }
    | { type: 'history', history: ConversationRerollHistory | null }
    | { type: 'none' }

export function handleDefaultChatUnreroll(
    options: DefaultChatUnrerollOptions,
): DefaultChatUnrerollResult {
    const generationId = options.target.conversation.message.at(-1)
        ?.generationInfo?.generationId
    if (generationId) {
        const replacement = options.preUnreroll(generationId)
        if (replacement) {
            replaceConversationRerollLastData(
                options.target,
                replacement,
                'unreroll',
            )
            return { type: 'precomputed' }
        }
    }

    const history = options.history
    if (!history || history.index <= 0) return { type: 'none' }
    return {
        type: 'history',
        history: moveConversationRerollHistory(
            history,
            options.target,
            'unreroll',
        ),
    }
}
