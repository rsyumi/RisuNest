import type { Chat, Message } from '../../ts/storage/database.svelte'
import {
    appendConversationMessage,
    captureConversationMutationTarget,
    type ConversationMutationTarget,
} from '../../ts/conversationMutations'
import type { ConversationCommitObserver } from '../../ts/process/conversationOperationContext'

interface DefaultChatInputOptions {
    target: ConversationMutationTarget
    runInputTrigger(
        onConversationCommit: ConversationCommitObserver,
    ): Promise<{ chat: Chat } | null | undefined>
    processInput(onConversationCommit: ConversationCommitObserver): Promise<string>
    isTargetCurrent(target: ConversationMutationTarget): boolean
    recaptureTarget?(): ConversationMutationTarget
    createMessage(data: string): Message
}

export async function appendDefaultChatInput(
    options: DefaultChatInputOptions,
): Promise<boolean> {
    let target = options.target
    let baseMessages = target.messages
    let invalidated = false
    const recapture = () =>
        options.recaptureTarget?.() ??
        captureConversationMutationTarget(target.character, target.conversation, target.session)
    const isCurrent = () => {
        if (invalidated) return false
        if (
            target.session &&
            target.sessionVersion !== target.session.version &&
            target.session.canContinueGenerationFrom(target.sessionVersion!)
        ) {
            const next = recapture()
            if (
                next.session !== target.session ||
                next.conversation !== target.conversation ||
                next.character.chaId !== target.character.chaId ||
                !options.isTargetCurrent(next)
            )
                return false
            target = next
            baseMessages = next.messages
        }
        return options.isTargetCurrent(target)
    }
    const onConversationCommit: ConversationCommitObserver = (commit) => {
        if (
            invalidated ||
            !commit.follows(target.session, target.sessionVersion, target.conversation)
        ) {
            invalidated = true
            return
        }
        const next = recapture()
        if (!options.isTargetCurrent(next)) {
            invalidated = true
            return
        }
        target = next
        baseMessages = next.messages
    }
    const triggerResult = await options.runInputTrigger(onConversationCommit)
    if (!isCurrent()) return false
    if (triggerResult) baseMessages = triggerResult.chat.message

    const processedInput = await options.processInput(onConversationCommit)
    if (!isCurrent()) return false
    appendConversationMessage(target, options.createMessage(processedInput), baseMessages)
    return true
}
