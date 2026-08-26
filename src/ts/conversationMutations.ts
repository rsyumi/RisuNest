import type { ActiveConversationSession } from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'

type ConversationCharacter = Database['characters'][number]

export interface ConversationMutationTarget {
    character: ConversationCharacter
    conversation: Chat
    session: ActiveConversationSession | null
}

export function captureConversationMutationTarget(
    character: ConversationCharacter,
    conversation: Chat,
    candidateSession: ActiveConversationSession | null,
): ConversationMutationTarget {
    const session = getMatchingConversationSession(character, conversation, candidateSession)
    return { character, conversation, session }
}

export function isConversationMutationTargetCurrent(
    target: ConversationMutationTarget,
    character: ConversationCharacter | undefined,
    conversation: Chat | undefined,
    candidateSession: ActiveConversationSession | null,
): boolean {
    return character === target.character &&
        conversation === target.conversation &&
        getMatchingConversationSession(character, conversation, candidateSession) === target.session
}

export function appendConversationMessage(
    target: ConversationMutationTarget,
    message: Message,
    baseMessages: Message[] = target.conversation.message,
): void {
    if (target.session) {
        if (baseMessages === target.conversation.message) {
            target.session.append(message)
        } else {
            target.session.transaction((transaction) => {
                transaction.replaceTail(transaction.positionAt(0), baseMessages)
                transaction.append(message)
            })
        }
    } else {
        if (baseMessages !== target.conversation.message) {
            target.conversation.message = baseMessages
        }
        target.conversation.message.push(message)
    }
}

export function appendConversationComment(
    target: ConversationMutationTarget,
    addition: string,
): void {
    const absoluteIndex = target.conversation.message.length - 1
    const message = target.conversation.message[absoluteIndex]
    if (!message) throw new TypeError("Cannot read properties of undefined (reading 'data')")
    if (target.session) {
        target.session.edit(target.session.locate(absoluteIndex), {
            ...message,
            data: message.data + addition,
        })
    } else {
        message.data += addition
    }
}

export function cutConversationMessages(
    target: ConversationMutationTarget,
    argument: string,
): void {
    if (argument.includes('-')) {
        const [start, end] = argument.split('-')
        replaceConversationMessages(
            target,
            target.conversation.message.slice(parseInt(start), parseInt(end)),
        )
        return
    }
    const index = parseInt(argument)
    if (!isNaN(index)) {
        const messages = target.conversation.message.slice()
        replaceConversationMessages(target, messages.splice(index, 1))
        return
    }
    replaceConversationMessages(
        target,
        target.conversation.message.filter((message) => message.chatId !== argument),
    )
}

export function retainConversationDeleteSlice(
    target: ConversationMutationTarget,
    argument: string,
): void {
    const size = parseInt(argument)
    if (isNaN(size)) return
    replaceConversationMessages(
        target,
        target.conversation.message.slice(target.conversation.message.length - size),
    )
}

export function resetConversationWithMessage(
    target: ConversationMutationTarget,
    message: Message,
): void {
    if (target.session) {
        target.session.transaction((transaction) => {
            transaction.replaceTail(transaction.positionAt(0), [])
            transaction.append(message)
        })
    } else {
        target.conversation.message = []
        target.conversation.message.push(message)
    }
}

function replaceConversationMessages(
    target: ConversationMutationTarget,
    messages: readonly Message[],
): void {
    if (target.session) {
        target.session.replaceTail(target.session.positionAt(0), messages)
    } else {
        target.conversation.message = [...messages]
    }
}

function getMatchingConversationSession(
    character: ConversationCharacter | undefined,
    conversation: Chat | undefined,
    candidateSession: ActiveConversationSession | null,
): ActiveConversationSession | null {
    if (!character || !conversation || !candidateSession?.isActive) return null
    return candidateSession.characterId === character.chaId &&
        candidateSession.conversationId === conversation.id &&
        candidateSession.materializeCompatibilityArray() === conversation.message
        ? candidateSession
        : null
}
