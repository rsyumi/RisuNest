import type { Chat, Message } from '../../ts/storage/database.svelte'
import {
    appendConversationMessage,
    type ConversationMutationTarget,
} from '../../ts/conversationMutations'

interface DefaultChatInputOptions {
    target: ConversationMutationTarget
    runInputTrigger(): Promise<{ chat: Chat } | null | undefined>
    processInput(): Promise<string>
    isTargetCurrent(): boolean
    createMessage(data: string): Message
}

export async function appendDefaultChatInput(
    options: DefaultChatInputOptions,
): Promise<boolean> {
    let baseMessages = options.target.messages
    const triggerResult = await options.runInputTrigger()
    if (!options.isTargetCurrent()) return false
    if (triggerResult) baseMessages = triggerResult.chat.message

    const processedInput = await options.processInput()
    if (!options.isTargetCurrent()) return false
    appendConversationMessage(
        options.target,
        options.createMessage(processedInput),
        baseMessages,
    )
    return true
}
