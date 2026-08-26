import type {
    ActiveConversationSession,
    ConversationPosition,
} from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'

type ConversationCharacter = Database['characters'][number]

export interface ConversationMutationTarget {
    character: ConversationCharacter
    conversation: Chat
    messages: Message[]
    messageCount: number
    session: ActiveConversationSession | null
    sessionVersion: number | null
    startPosition: ConversationPosition | null
}

export class ConversationMutationTargetStaleError extends Error {
    constructor() {
        super('Conversation mutation target is stale')
        this.name = 'ConversationMutationTargetStaleError'
    }
}

export function captureConversationMutationTarget(
    character: ConversationCharacter,
    conversation: Chat,
    candidateSession: ActiveConversationSession | null,
): ConversationMutationTarget {
    const session = getMatchingConversationSession(character, conversation, candidateSession)
    return {
        character,
        conversation,
        messages: conversation.message,
        messageCount: conversation.message.length,
        session,
        sessionVersion: session?.version ?? null,
        startPosition: session?.positionAt(0) ?? null,
    }
}

export function isConversationMutationTargetCurrent(
    target: ConversationMutationTarget,
    character: ConversationCharacter | undefined,
    conversation: Chat | undefined,
    candidateSession: ActiveConversationSession | null,
): boolean {
    return isConversationMutationOwnerCurrent(
        target,
        character,
        conversation,
        candidateSession,
    ) && isConversationMutationTargetStateCurrent(target)
}

export function isConversationMutationOwnerCurrent(
    target: ConversationMutationTarget,
    character: ConversationCharacter | undefined,
    conversation: Chat | undefined,
    candidateSession: ActiveConversationSession | null,
): boolean {
    return character === target.character &&
        conversation === target.conversation &&
        getMatchingConversationSession(character, conversation, candidateSession) === target.session
}

export function refreshConversationMutationTarget(
    target: ConversationMutationTarget,
    character: ConversationCharacter | undefined,
    conversation: Chat | undefined,
    candidateSession: ActiveConversationSession | null,
): ConversationMutationTarget | null {
    if (!character || !conversation || !isConversationMutationOwnerCurrent(
        target,
        character,
        conversation,
        candidateSession,
    )) return null
    return captureConversationMutationTarget(character, conversation, candidateSession)
}

export function appendConversationMessage(
    target: ConversationMutationTarget,
    message: Message,
    baseMessages: Message[] = target.messages,
): void {
    assertConversationMutationTargetCurrent(target)
    if (target.session) {
        if (baseMessages === target.messages) {
            target.session.append(message)
        } else {
            target.session.transaction((transaction) => {
                transaction.replaceTail(target.startPosition!, baseMessages)
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

export function appendCurrentConversationMessage(
    character: ConversationCharacter,
    conversation: Chat,
    candidateSession: ActiveConversationSession | null,
    message: Message,
): void {
    const target = captureConversationMutationTarget(
        character,
        conversation,
        candidateSession,
    )
    if (candidateSession && target.session !== candidateSession) {
        throw new ConversationMutationTargetStaleError()
    }
    appendConversationMessage(target, message)
}

export function ensureCurrentConversationMessageIds(
    character: ConversationCharacter,
    conversation: Chat,
    candidateSession: ActiveConversationSession | null,
    createId: () => string,
): number {
    const target = captureConversationMutationTarget(
        character,
        conversation,
        candidateSession,
    )
    if (candidateSession && target.session !== candidateSession) {
        throw new ConversationMutationTargetStaleError()
    }
    if (target.session) return target.session.ensureNullishMessageIds(createId)

    let assigned = 0
    target.conversation.message = target.conversation.message.map((message) => {
        if (message.chatId !== undefined && message.chatId !== null) return message
        const id = createId()
        if (!id) throw new Error('Message ID generator returned an empty ID')
        assigned += 1
        return { ...message, chatId: id }
    })
    return assigned
}

export function appendConversationComment(
    target: ConversationMutationTarget,
    addition: string,
): void {
    assertConversationMutationTargetCurrent(target)
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
    assertConversationMutationTargetCurrent(target)
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
    assertConversationMutationTargetCurrent(target)
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
    assertConversationMutationTargetCurrent(target)
    if (target.session) {
        target.session.transaction((transaction) => {
            transaction.replaceTail(target.startPosition!, [])
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
    assertConversationMutationTargetCurrent(target)
    if (target.session) {
        target.session.replaceTail(target.startPosition!, messages)
    } else {
        target.conversation.message = [...messages]
    }
}

export function assertConversationMutationTargetCurrent(target: ConversationMutationTarget): void {
    if (!isConversationMutationTargetStateCurrent(target)) {
        throw new ConversationMutationTargetStaleError()
    }
}

function isConversationMutationTargetStateCurrent(target: ConversationMutationTarget): boolean {
    if (target.conversation.message !== target.messages) return false
    if (target.conversation.message.length !== target.messageCount) return false
    if (!target.session) return target.sessionVersion === null && target.startPosition === null
    return target.session.isActive &&
        target.session.version === target.sessionVersion &&
        target.session.materializeCompatibilityArray() === target.messages
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
