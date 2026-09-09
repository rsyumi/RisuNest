import { precomputedResponseVariants } from '../responseVariants'
import type { ActiveConversationSession } from '../storage/activeConversationSession'
import type {
    Chat,
    Message,
    MessageGenerationInfo,
    MessagePresetInfo,
    StreamingDisplayOptimizationMode,
} from '../storage/database.svelte'
import {
    captureGenerationConversationOperation,
    captureGenerationTailFallbackOperation,
    recaptureGenerationConversationOperation,
    type GenerationConversationOperation,
} from './generationConversationOperation'
import type { requestDataResponse } from './request/request'
import { consumeStreamingDisplayStream } from './streamingDisplayStream'
import type { ConversationCommitObserver } from './conversationOperationContext'

export interface GenerationResponseProcessingOptions {
    onConversationCommit?: ConversationCommitObserver
    cache: 'normal' | 'bypass'
    signal: AbortSignal
    regexWorker: true
}

export interface GenerationResponseOperationContext {
    getCurrentSession(): ActiveConversationSession | null
    getTargetChat(): Chat | null | undefined
    isOwnerCurrent(): boolean
    publishTargetChat(chat: Chat): void
    invalidateSession(): void
    incrementReloadKeys(): void
}

export interface GenerationResponseCallbacks {
    reformatContent(data: string): string
    processOutput(
        data: string,
        messageIndex: number,
        options?: GenerationResponseProcessingOptions,
    ): Promise<{ data: string; emoChanged: boolean }>
    runCurrentChatParser(chat: Chat): Chat
    runInlay(data: string): { text: string; promise?: Promise<string> }
    runOutputTrigger(
        chat: Chat,
        onConversationCommit?: ConversationCommitObserver,
    ): Promise<
        | {
              chat?: Chat
              sendAIprompt?: boolean
          }
        | null
        | undefined
    >
    runOutputListeners(chat: Chat, messageIndex: number): Promise<void>
    speak(data: string): Promise<void>
    addRerolls(generationId: string, values: string[]): void
    trimIncompleteResponse(data: string): string
    markResponseApplied(): void
    onProviderFailure(message: string): void
}

export interface ApplyGenerationResponseOptions {
    response: requestDataResponse
    abortSignal: AbortSignal
    continueGeneration: boolean
    sayingCharacterId: string
    generationId: string
    generationInfo: MessageGenerationInfo
    promptInfo: MessagePresetInfo
    removeIncompleteResponse(): boolean
    streamingDisplayOptimizationMode(): StreamingDisplayOptimizationMode
    ttsAutoSpeech(): boolean
    operation: GenerationResponseOperationContext
    callbacks: GenerationResponseCallbacks
}

export interface GenerationResponseApplication {
    readonly result: string
    readonly emoChanged: boolean
    readonly resendChat: boolean
    hasOutputOwnership(): boolean
    readOutput(): Message | null
    updateOutput(update: (message: Message) => Message): boolean
    release(): void
}

export async function applyGenerationResponse(
    options: ApplyGenerationResponseOptions,
): Promise<GenerationResponseApplication | null> {
    const isOwnerCurrent = () => !options.abortSignal.aborted
        && options.operation.isOwnerCurrent()
    let outputTarget: GenerationConversationOperation | null = null
    let generationHadOperation = false
    const generationChat = options.operation.getTargetChat()
    const legacyFallbackTarget = options.response.type === 'multiline'
        && options.response.result.length === 0
        && generationChat
        ? captureGenerationTailFallbackOperation({
            session: options.operation.getCurrentSession(),
            getCurrentSession: options.operation.getCurrentSession,
            chat: generationChat,
            getCurrentChat: options.operation.getTargetChat,
            isOwnerCurrent,
        })
        : null
    const getOutputTarget = () => outputTarget ?? legacyFallbackTarget
    const readOutput = (): Message | null => getOutputTarget()?.snapshot() ?? null
    const updateOutput = (update: (message: Message) => Message): boolean => {
        const target = getOutputTarget()
        if (!target) return false
        const message = target.snapshot()
        return message !== null && target.commitMessage(update(message))
    }
    const hasOutputOwnership = () => getOutputTarget()?.isOwned() ?? false
    const onConversationCommit: ConversationCommitObserver = (commit) => {
        // The receipt proves that this callback committed from our captured version.
        // An unrelated mutation still leaves the old locator invalid.
        getOutputTarget()?.acceptCommit(commit)
    }
    let released = false
    const release = () => {
        if (released) return
        released = true
        outputTarget?.release()
        legacyFallbackTarget?.release()
        outputTarget = null
    }
    let handedOff = false
    let result = ''
    let emoChanged = false
    let resendChat = false

    try {
        if (options.abortSignal.aborted) return null
        if (options.response.type === 'fail') {
            options.callbacks.onProviderFailure(options.response.result)
            return null
        }

        if (options.response.type === 'streaming') {
            const reader = options.response.result.getReader()
            const targetChat = options.operation.getTargetChat()
            if (!targetChat) return null
            outputTarget = captureGenerationConversationOperation({
                session: options.operation.getCurrentSession(),
                getCurrentSession: options.operation.getCurrentSession,
                chat: targetChat,
                getCurrentChat: options.operation.getTargetChat,
                isOwnerCurrent,
                ...(options.continueGeneration ? { continueLast: true } : {
                    append: {
                        role: 'char',
                        data: '',
                        saying: options.sayingCharacterId,
                        time: Date.now(),
                        generationInfo: options.generationInfo,
                        promptInfo: options.promptInfo,
                        chatId: options.generationId,
                    },
                }),
            })
            generationHadOperation = true
            const initialOutput = outputTarget.snapshot()
            if (initialOutput === null) return null
            const prefix = options.continueGeneration ? initialOutput.data : ''
            const outputMessageId = outputTarget.messageId
            const performanceMode = options.streamingDisplayOptimizationMode()
            targetChat.isStreaming = true
            targetChat.activeStreamingDisplayOptimizationMode = performanceMode
            options.operation.incrementReloadKeys()
            let lastResponseChunk: Record<string, string> = {}
            const processStreamingSnapshot = async (
                snapshot: string,
                cache: 'normal' | 'bypass',
                signal: AbortSignal,
            ) => {
                try {
                    return await options.callbacks.processOutput(
                        prefix + snapshot,
                        outputTarget!.absoluteIndex,
                        { cache, signal, regexWorker: true, onConversationCommit },
                    )
                } catch (error) {
                    if (signal.aborted) return null
                    if (
                        outputTarget?.commitData(
                            options.callbacks.reformatContent(prefix + snapshot),
                        )
                    ) {
                        options.callbacks.markResponseApplied()
                        options.operation.incrementReloadKeys()
                    }
                    throw error
                }
            }
            let streamCompleted = false
            try {
                const streamResult = await consumeStreamingDisplayStream({
                    mode: performanceMode,
                    reader,
                    abortSignal: options.abortSignal,
                    isOwned: outputTarget.isOwned,
                    getSnapshot: (value) => {
                        const firstChunkKey = Object.keys(value)[0]
                        const snapshot = value[firstChunkKey] || ''
                        return options.removeIncompleteResponse()
                            ? options.callbacks.trimIncompleteResponse(snapshot)
                            : snapshot
                    },
                    onValue: (value, snapshot) => {
                        lastResponseChunk = value
                        result = snapshot
                    },
                    processSemantic: async ({ value }, context) => {
                        const cache = performanceMode === 'strong' ? 'normal' : 'bypass'
                        const processed = await processStreamingSnapshot(
                            value,
                            cache,
                            context.signal,
                        )
                        if (processed === null || !context.canCommit()) return
                        if (!outputTarget?.commitData(processed.data)) return
                        options.callbacks.markResponseApplied()
                        emoChanged = processed.emoChanged
                        options.operation.incrementReloadKeys()
                    },
                    processPreview: async ({ value }, context) => {
                        if (!context.canCommit()) return
                        if (!outputTarget?.commitData(
                            options.callbacks.reformatContent(prefix + value),
                        )) return
                        options.callbacks.markResponseApplied()
                        options.operation.incrementReloadKeys()
                    },
                })
                streamCompleted = streamResult.completed
            } finally {
                targetChat.isStreaming = false
                targetChat.activeStreamingDisplayOptimizationMode = undefined
                options.operation.incrementReloadKeys()
            }

            if (!streamCompleted) return null

            options.callbacks.addRerolls(
                options.generationId,
                Object.values(lastResponseChunk),
            )

            if (!outputTarget.isOwned()) return null
            let currentChat = options.callbacks.runCurrentChatParser(targetChat)
            options.operation.publishTargetChat(currentChat)
            if (!outputTarget.refresh()) return null
            const triggerResult = await options.callbacks.runOutputTrigger(
                currentChat,
                onConversationCommit,
            )
            if (!outputTarget.isOwned()) return null
            if (triggerResult?.chat) currentChat = triggerResult.chat
            if (triggerResult?.sendAIprompt) resendChat = true
            outputTarget.release()
            if (!outputMessageId) return null
            const previousChat = targetChat
            if (currentChat !== previousChat) options.operation.invalidateSession()
            options.operation.publishTargetChat(currentChat)
            const publishedChat = options.operation.getTargetChat()
            if (!publishedChat || !isOwnerCurrent()) return null
            currentChat = publishedChat
            outputTarget = recaptureGenerationConversationOperation({
                session: options.operation.getCurrentSession(),
                getCurrentSession: options.operation.getCurrentSession,
                chat: currentChat,
                getCurrentChat: options.operation.getTargetChat,
                isOwnerCurrent,
                messageId: outputMessageId,
            })
            if (!outputTarget || !outputTarget.isOwned()) return null
            const outputMessage = outputTarget.snapshot()
            if (outputMessage) {
                const inlay = options.callbacks.runInlay(outputMessage.data)
                if (!outputTarget.commitData(inlay.text)) return null
                if (inlay.promise) {
                    const inlayData = await inlay.promise
                    if (!outputTarget.commitData(inlayData)) return null
                }
            }
            const outputChat = options.operation.getTargetChat()
            if (!outputChat || !outputTarget.isOwned()) return null
            await options.callbacks.runOutputListeners(
                outputChat,
                outputTarget.absoluteIndex,
            )
            if (!outputTarget.isOwned()) return null
            if (options.ttsAutoSpeech()) await options.callbacks.speak(result)
        } else {
            const messages = options.response.type === 'success'
                ? [['char', options.response.result]] as const
                : options.response.type === 'multiline'
                    ? options.response.result
                    : []
            const multilineRerolls: string[] = []
            let outputMessageId: string | undefined
            for (let index = 0; index < messages.length; index++) {
                const message = messages[index]
                const messageText = message[1]
                const operationChat = options.operation.getTargetChat()
                if (!operationChat || !isOwnerCurrent()) return null
                let messageIndex = operationChat.message.length
                const continuingFirstMessage = index === 0 && options.continueGeneration
                let continueBaseData = ''
                if (continuingFirstMessage) {
                    outputTarget = captureGenerationConversationOperation({
                        session: options.operation.getCurrentSession(),
                        getCurrentSession: options.operation.getCurrentSession,
                        chat: operationChat,
                        getCurrentChat: options.operation.getTargetChat,
                        isOwnerCurrent,
                        continueLast: true,
                    })
                    generationHadOperation = true
                    const beforeChat = outputTarget.snapshot()
                    if (!beforeChat) return null
                    continueBaseData = beforeChat.data
                }
                let processed: Awaited<ReturnType<GenerationResponseCallbacks['processOutput']>>
                try {
                    processed = await options.callbacks.processOutput(messageText, messageIndex, {
                        cache: 'normal',
                        signal: options.abortSignal,
                        regexWorker: true,
                        onConversationCommit,
                    })
                    if (!isOwnerCurrent()) return null
                    if (continuingFirstMessage) {
                        messageIndex = outputTarget!.absoluteIndex
                        processed = await options.callbacks.processOutput(
                            continueBaseData + messageText,
                            messageIndex,
                            {
                                cache: 'normal',
                                signal: options.abortSignal,
                                regexWorker: true,
                                onConversationCommit,
                            },
                        )
                        if (!isOwnerCurrent()) return null
                    }
                } catch (error) {
                    if (options.abortSignal.aborted) return null
                    const fallbackData = options.callbacks.reformatContent(
                        continueBaseData + messageText,
                    )
                    let applied = false
                    try {
                        if (continuingFirstMessage) {
                            applied =
                                outputTarget?.commitMessage({
                                    ...outputTarget.snapshot(),
                                    role: 'char',
                                    data: fallbackData,
                                    saying: options.sayingCharacterId,
                                    time: Date.now(),
                                    generationInfo: options.generationInfo,
                                    promptInfo: options.promptInfo,
                                    chatId: options.generationId,
                                }) ?? false
                            outputMessageId = outputTarget?.messageId
                        } else if (
                            index === 0
                            && options.operation.getTargetChat() === operationChat
                            && isOwnerCurrent()
                        ) {
                            outputTarget = captureGenerationConversationOperation({
                                session: options.operation.getCurrentSession(),
                                getCurrentSession: options.operation.getCurrentSession,
                                chat: operationChat,
                                getCurrentChat: options.operation.getTargetChat,
                                isOwnerCurrent,
                                append: {
                                    role: message[0],
                                    data: fallbackData,
                                    saying: options.sayingCharacterId,
                                    time: Date.now(),
                                    generationInfo: options.generationInfo,
                                    promptInfo: options.promptInfo,
                                    chatId: options.generationId,
                                },
                            })
                            generationHadOperation = true
                            applied = outputTarget.isOwned()
                            outputMessageId = outputTarget.messageId
                        } else {
                            applied = outputTarget?.commitData(fallbackData) ?? false
                        }
                        if (applied) {
                            result = fallbackData
                            options.callbacks.markResponseApplied()
                            options.operation.incrementReloadKeys()
                        }
                    } catch {}
                    throw error
                }

                if (options.removeIncompleteResponse()) {
                    processed.data = options.callbacks.trimIncompleteResponse(processed.data)
                }
                result = processed.data
                const inlay = options.callbacks.runInlay(result)
                result = inlay.text
                emoChanged = processed.emoChanged
                if (continuingFirstMessage) {
                    if (
                        !outputTarget?.commitMessage({
                            ...outputTarget.snapshot(),
                            role: 'char',
                            data: result,
                            saying: options.sayingCharacterId,
                            time: Date.now(),
                            generationInfo: options.generationInfo,
                            promptInfo: options.promptInfo,
                            chatId: options.generationId,
                        })
                    )
                        return null
                    options.callbacks.markResponseApplied()
                    if (inlay.promise) {
                        const inlayData = await inlay.promise
                        if (!outputTarget.commitData(inlayData)) return null
                    }
                    outputMessageId = outputTarget.messageId
                } else if (index === 0) {
                    if (
                        options.operation.getTargetChat() !== operationChat
                        || !isOwnerCurrent()
                    ) return null
                    outputTarget = captureGenerationConversationOperation({
                        session: options.operation.getCurrentSession(),
                        getCurrentSession: options.operation.getCurrentSession,
                        chat: operationChat,
                        getCurrentChat: options.operation.getTargetChat,
                        isOwnerCurrent,
                        append: {
                            role: message[0],
                            data: result,
                            saying: options.sayingCharacterId,
                            time: Date.now(),
                            generationInfo: options.generationInfo,
                            promptInfo: options.promptInfo,
                            chatId: options.generationId,
                        },
                    })
                    generationHadOperation = true
                    if (!outputTarget.isOwned()) return null
                    options.callbacks.markResponseApplied()
                    if (inlay.promise) {
                        const inlayData = await inlay.promise
                        if (!outputTarget.commitData(inlayData)) return null
                    }
                    multilineRerolls.push(result)
                    outputMessageId = outputTarget.messageId
                } else {
                    multilineRerolls.push(result)
                }
                options.operation.incrementReloadKeys()
                if (options.ttsAutoSpeech()) await options.callbacks.speak(result)
            }

            if (multilineRerolls.length > 1) {
                const message = outputTarget?.snapshot()
                if (
                    message &&
                    !outputTarget!.commitMessage(precomputedResponseVariants(message, multilineRerolls))
                )
                    return null
                options.callbacks.addRerolls(options.generationId, multilineRerolls)
            }

            const outputChat = options.operation.getTargetChat()
            if (!outputChat || !isOwnerCurrent()) return null
            if (outputTarget && !outputTarget.isOwned()) return null
            let currentChat = options.callbacks.runCurrentChatParser(outputChat)
            options.operation.publishTargetChat(currentChat)
            if (!getOutputTarget()?.refresh()) return null
            const triggerResult = await options.callbacks.runOutputTrigger(
                currentChat,
                onConversationCommit,
            )
            if (!hasOutputOwnership()) return null
            if (triggerResult?.sendAIprompt) resendChat = true
            const previousChat = currentChat
            const nextChat = triggerResult?.chat ?? currentChat
            outputTarget?.release()
            if (nextChat !== previousChat) options.operation.invalidateSession()
            options.operation.publishTargetChat(nextChat)
            const publishedChat = options.operation.getTargetChat()
            if (!publishedChat || !isOwnerCurrent()) return null
            currentChat = publishedChat
            if (generationHadOperation) {
                if (!outputMessageId) return null
                outputTarget = recaptureGenerationConversationOperation({
                    session: options.operation.getCurrentSession(),
                    getCurrentSession: options.operation.getCurrentSession,
                    chat: currentChat,
                    getCurrentChat: options.operation.getTargetChat,
                    isOwnerCurrent,
                    messageId: outputMessageId,
                })
                if (!outputTarget || !outputTarget.isOwned()) return null
                await options.callbacks.runOutputListeners(
                    currentChat,
                    outputTarget.absoluteIndex,
                )
                if (!outputTarget.isOwned()) return null
            }
        }

        const application: GenerationResponseApplication = {
            result,
            emoChanged,
            resendChat,
            hasOutputOwnership,
            readOutput,
            updateOutput,
            release,
        }
        handedOff = true
        return application
    } finally {
        if (!handedOff) release()
    }
}
