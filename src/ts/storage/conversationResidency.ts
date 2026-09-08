import type { Chat } from './database.svelte'
import type { ConversationSummary } from './persistentDataStore'

const conversationSummaryStub = Symbol('conversationSummaryStub')

export function createConversationSummaryStub(summary: ConversationSummary): Chat {
    const stub: Chat = {
        id: summary.id,
        name: summary.name,
        folderId: summary.folderId,
        bindedPersona: summary.bindedPersona,
        note: '',
        localLore: [],
        message: [],
        lastDate: summary.recentAt,
        ...(summary.fmIndex === undefined ? {} : { fmIndex: summary.fmIndex }),
    }
    Object.defineProperty(stub, conversationSummaryStub, {
        configurable: false,
        enumerable: false,
        value: summary,
        writable: false,
    })
    return stub
}

export function createConversationSummaryStubFromChat(
    characterId: string,
    conversation: Chat,
    configuredIndex: number,
): Chat {
    return createConversationSummaryStub(
        createConversationSummaryFromMetadata(
            characterId,
            conversation,
            configuredIndex,
            conversation.message.length,
            conversation.lastDate ?? conversation.message.at(-1)?.time ?? 0,
        ),
    )
}

export function createConversationSummaryFromMetadata(
    characterId: string,
    conversation: Omit<Chat, 'message'>,
    configuredIndex: number,
    messageCount: number,
    recentAtFallback: number,
    retained?: ConversationSummary,
): ConversationSummary {
    return {
        ...retained,
        id: conversation.id!,
        characterId,
        name: conversation.name,
        folderId: conversation.folderId,
        bindedPersona: conversation.bindedPersona,
        configuredIndex: retained?.configuredIndex ?? configuredIndex,
        recentAt: conversation.lastDate ?? recentAtFallback,
        messageCount,
        ...((conversation.fmIndex ?? retained?.fmIndex) === undefined
            ? {}
            : { fmIndex: conversation.fmIndex ?? retained?.fmIndex }),
    }
}

export function isConversationSummaryStub(conversation: Chat): boolean {
    return conversationSummaryStub in conversation
}

export function getConversationSummaryStub(
    conversation: Chat,
): ConversationSummary | null {
    if (!isConversationSummaryStub(conversation)) return null
    return (
        conversation as Chat & {
            readonly [conversationSummaryStub]: ConversationSummary
        }
    )[conversationSummaryStub]
}
