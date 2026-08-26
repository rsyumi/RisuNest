import {
    requireCurrentConversationSession,
    type ActiveConversationSession,
    type MessageLocator,
} from './storage/activeConversationSession'
import type { Chat, Database, Message } from './storage/database.svelte'

export interface CurrentChatRemovalTarget {
    character: Database['characters'][number]
    conversation: Chat
}

export interface RemoveChatMessageOptions {
    absoluteIndex: number
    shiftKey: boolean
    recursive: boolean
    askRemoval: boolean
    instantRemove: boolean
    captureCurrent(): CurrentChatRemovalTarget | null
    getCurrentSession(): ActiveConversationSession | null
    confirmRemoval(): Promise<boolean>
    confirmInstantRemoval(): Promise<boolean>
}

interface CapturedChatRemovalTarget extends CurrentChatRemovalTarget {
    messages: Message[]
    message: Message
    session: ActiveConversationSession | null
    locator: MessageLocator | null
}

export async function removeChatMessage(options: RemoveChatMessageOptions): Promise<boolean> {
    const current = options.captureCurrent()
    const message = current?.conversation.message[options.absoluteIndex]
    if (!current || !message) return false
    const session = options.getCurrentSession()
    const target: CapturedChatRemovalTarget = {
        ...current,
        messages: current.conversation.message,
        message,
        session,
        locator: session?.locate(options.absoluteIndex) ?? null,
    }

    if (options.shiftKey) return mutateCapturedTarget(options, target, 'truncate')

    if (options.askRemoval && !await options.confirmRemoval()) return false
    if (!isCurrentTarget(options, target)) return false

    if (options.instantRemove || options.recursive) {
        const removeOnlySelected = await options.confirmInstantRemoval()
        if (!isCurrentTarget(options, target)) return false
        return mutateCapturedTarget(
            options,
            target,
            removeOnlySelected ? 'delete' : 'truncate',
        )
    }
    return mutateCapturedTarget(options, target, 'delete')
}

function isCurrentTarget(
    options: RemoveChatMessageOptions,
    target: CapturedChatRemovalTarget,
): boolean {
    const current = options.captureCurrent()
    return (
        current?.character === target.character &&
        current.conversation === target.conversation &&
        current.conversation.message === target.messages &&
        current.conversation.message[options.absoluteIndex] === target.message &&
        options.getCurrentSession() === target.session
    )
}

function mutateCapturedTarget(
    options: RemoveChatMessageOptions,
    target: CapturedChatRemovalTarget,
    mutation: 'delete' | 'truncate',
): boolean {
    if (!isCurrentTarget(options, target)) return false
    if (target.session && target.locator) {
        const session = requireCurrentConversationSession(
            target.session,
            options.getCurrentSession(),
        )
        if (mutation === 'delete') session.delete(target.locator)
        else session.truncate(target.locator)
        return true
    }
    if (mutation === 'delete') {
        target.conversation.message.splice(options.absoluteIndex, 1)
        target.conversation.message = target.conversation.message
    } else {
        target.conversation.message = target.conversation.message.slice(
            0,
            options.absoluteIndex,
        )
    }
    return true
}
