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
    return createConversationSummaryStub({
        id: conversation.id!,
        characterId,
        name: conversation.name,
        folderId: conversation.folderId,
        bindedPersona: conversation.bindedPersona,
        configuredIndex,
        recentAt: conversation.lastDate ?? conversation.message.at(-1)?.time ?? 0,
        messageCount: conversation.message.length,
    })
}

export function isConversationSummaryStub(conversation: Chat): boolean {
    return conversationSummaryStub in conversation
}
