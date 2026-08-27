import {
    cloneConversationMetadata,
    ConversationSessionInactiveError,
    ConversationSessionStaleError,
    type ActiveConversationOperationRange,
    type ActiveConversationSession,
} from '../storage/activeConversationSession'
import type {
    Chat,
    Database,
    Message,
    character,
    groupChat,
} from '../storage/database.svelte'
import { getChatVarFromConversation, setChatVarOnConversation } from '../parser/chatVar.svelte'
import { safeStructuredClone } from '../polyfill'

const CURRENT_CHAT_PARSE_PAGE_SIZE = 128

export interface CurrentChatParserContext {
    chara: character | groupChat
    runVar: true
    db?: Database
    selectedCharacterId?: string
    getChatVar?(key: string): string
    setChatVar?(key: string, value: string): void
}

export type CurrentChatParser = (
    data: string,
    context: CurrentChatParserContext,
) => string

export interface RunCurrentChatParserPassOptions {
    chat: Chat
    database: Database
    ownerCharacterId: string
    parserCharacter: character | groupChat
    session: ActiveConversationSession | null
    parser: CurrentChatParser
}

export function runCurrentChatParserPass(
    options: RunCurrentChatParserPassOptions,
): Chat {
    const { chat, session } = options
    if (!session) {
        chat.message = chat.message.map((message) => {
            message.data = options.parser(message.data, {
                chara: options.parserCharacter,
                runVar: true,
            })
            return message
        })
        return chat
    }
    if (!session.matchesConversation(options.ownerCharacterId, chat)) {
        throw new ConversationSessionInactiveError()
    }

    const pin = session.acquirePin('compatibility')
    try {
        const expectedVersion = session.version
        const expectedMetadata = cloneConversationMetadata(chat)
        const projectedMessages = chat.message.slice()
        const projectedChat = {
            ...safeStructuredClone(expectedMetadata),
            message: projectedMessages,
        } as unknown as Chat
        const operationDatabase = createConversationDatabaseView(
            options.database,
            options.ownerCharacterId,
            chat,
            projectedChat,
        )
        const parserCharacter = options.parserCharacter.chaId === options.ownerCharacterId
            ? operationDatabase.characters.find(
                (candidate) => candidate.chaId === options.ownerCharacterId,
            ) ?? options.parserCharacter
            : options.parserCharacter
        const ranges: ActiveConversationOperationRange[] = []

        for (
            let startIndex = 0;
            startIndex < session.totalMessages;
            startIndex += CURRENT_CHAT_PARSE_PAGE_SIZE
        ) {
            const limit = Math.min(
                CURRENT_CHAT_PARSE_PAGE_SIZE,
                session.totalMessages - startIndex,
            )
            const window = session.readRange(startIndex, limit)
            if (window.sessionVersion !== expectedVersion) {
                throw new ConversationSessionStaleError(
                    expectedVersion,
                    window.sessionVersion,
                )
            }
            const projectedPage = window.messages.slice()
            let firstChangedOffset = -1
            let lastChangedOffset = -1
            for (let offset = 0; offset < window.messages.length; offset++) {
                const absoluteIndex = window.startIndex + offset
                const original = window.messages[offset]
                const parsedData = options.parser(original.data, {
                    chara: parserCharacter,
                    runVar: true,
                    db: operationDatabase,
                    selectedCharacterId: options.ownerCharacterId,
                    getChatVar: (key) => getChatVarFromConversation(
                        operationDatabase,
                        options.ownerCharacterId,
                        projectedChat,
                        key,
                    ),
                    setChatVar: (key, value) => {
                        setChatVarOnConversation(projectedChat, key, value)
                    },
                })
                if (parsedData === original.data) continue
                const replacement = {
                    ...original,
                    data: parsedData,
                }
                projectedMessages[absoluteIndex] = replacement
                projectedPage[offset] = replacement
                if (firstChangedOffset === -1) firstChangedOffset = offset
                lastChangedOffset = offset
            }
            if (firstChangedOffset !== -1) {
                const endOffset = lastChangedOffset + 1
                ranges.push({
                    position: session.positionAt(window.startIndex + firstChangedOffset),
                    deleteCount: endOffset - firstChangedOffset,
                    expectedMessages: window.messages.slice(firstChangedOffset, endOffset),
                    messages: projectedPage.slice(firstChangedOffset, endOffset),
                })
            }
        }
        session.applyOperation({
            expectedVersion,
            expectedMetadata,
            metadata: cloneConversationMetadata(projectedChat),
            ranges,
        })
        return chat
    } finally {
        pin.release()
    }
}

function createConversationDatabaseView(
    database: Database,
    ownerCharacterId: string,
    sourceChat: Chat,
    projectedChat: Chat,
): Database {
    const characterIndex = database.characters.findIndex(
        (candidate) => candidate.chaId === ownerCharacterId,
    )
    if (characterIndex === -1) return database
    const character = database.characters[characterIndex]
    const conversationIndex = character.chats.findIndex(
        (candidate) => candidate === sourceChat,
    )
    if (conversationIndex === -1) return database
    const characters = database.characters.slice()
    const chats = character.chats.slice()
    chats[conversationIndex] = projectedChat
    characters[characterIndex] = { ...character, chats }
    return { ...database, characters }
}
