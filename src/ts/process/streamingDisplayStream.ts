import type { StreamingDisplayOptimizationMode } from '../storage/database.svelte'
import {
    createStreamingDisplayController,
    type ScheduledSnapshot,
    type StreamingDisplayProcessContext,
} from './streamingDisplayScheduler'

interface StreamingTargetChat<TMessage> {
    id?: string
    message: TMessage[]
}

interface StreamingTargetCharacter<TChat> {
    chaId: string
    chats: TChat[]
}

export interface StreamingMessageTarget<TCharacter, TChat, TMessage> {
    character: TCharacter
    chat: TChat
    message: TMessage
    isOwned(): boolean
}

export function captureStreamingMessageTarget<
    TCharacter extends StreamingTargetCharacter<TChat>,
    TChat extends StreamingTargetChat<TMessage>,
    TMessage,
>(
    getCharacters: () => readonly TCharacter[],
    characterIndex: number,
    chatIndex: number,
    messageIndex: number,
): StreamingMessageTarget<TCharacter, TChat, TMessage> {
    const character = getCharacters()[characterIndex]
    const chat = character.chats[chatIndex]
    const message = chat.message[messageIndex]

    return {
        character,
        chat,
        message,
        isOwned() {
            const currentCharacter = getCharacters()[characterIndex]
            const currentChat = currentCharacter?.chats[chatIndex]
            return currentCharacter === character
                && currentChat === chat
                && currentChat?.message[messageIndex] === message
        },
    }
}

export interface StreamingDisplayReader<T> {
    read(): Promise<ReadableStreamReadResult<T>>
    cancel(): Promise<void>
}

interface ConsumeStreamingDisplayStreamOptions<T> {
    mode: StreamingDisplayOptimizationMode
    reader: StreamingDisplayReader<T>
    abortSignal: AbortSignal
    getSnapshot(value: T): string
    isOwned(): boolean
    processSemantic(
        snapshot: ScheduledSnapshot<string>,
        context: StreamingDisplayProcessContext,
    ): Promise<void>
    processPreview(
        snapshot: ScheduledSnapshot<string>,
        context: StreamingDisplayProcessContext,
    ): Promise<void>
    onValue?(value: T, snapshot: string): void
}

interface ConsumeStreamingDisplayStreamResult<T> {
    completed: boolean
    latestSnapshot: string
    lastValue: T | undefined
}

export async function consumeStreamingDisplayStream<T>(
    options: ConsumeStreamingDisplayStreamOptions<T>,
): Promise<ConsumeStreamingDisplayStreamResult<T>> {
    let aborted = options.abortSignal.aborted
    let normalEof = false
    let latestSnapshot = ''
    let lastValue: T | undefined
    const withOwnership = (
        context: StreamingDisplayProcessContext,
    ): StreamingDisplayProcessContext => ({
        signal: context.signal,
        canCommit: () => context.canCommit() && options.isOwned(),
    })
    const controller = createStreamingDisplayController<string>({
        mode: options.mode,
        processSemantic: (snapshot, context) =>
            options.processSemantic(snapshot, withOwnership(context)),
        processPreview: (snapshot, context) =>
            options.processPreview(snapshot, withOwnership(context)),
        onError: () => {
            void options.reader.cancel().catch(() => {})
        },
    })
    const abort = () => {
        aborted = true
        void controller.abort()
        void options.reader.cancel().catch(() => {})
    }

    options.abortSignal.addEventListener('abort', abort, { once: true })
    if (options.abortSignal.aborted) abort()
    try {
        while (!aborted) {
            let read: ReadableStreamReadResult<T>
            try {
                read = await options.reader.read()
            }
            catch (error) {
                if (options.abortSignal.aborted || aborted) break
                await controller.abort()
                throw error
            }
            if (read.value !== undefined) {
                lastValue = read.value
                latestSnapshot = options.getSnapshot(read.value)
                options.onValue?.(read.value, latestSnapshot)
                await controller.submit(latestSnapshot)
            }
            if (read.done) {
                normalEof = true
                break
            }
        }
    }
    finally {
        try {
            if (normalEof && !aborted && !options.abortSignal.aborted) {
                await controller.finish()
            }
            else {
                await controller.abort()
            }
        }
        finally {
            options.abortSignal.removeEventListener('abort', abort)
            void options.reader.cancel().catch(() => {})
        }
    }

    return {
        completed: normalEof && !aborted && options.isOwned(),
        latestSnapshot,
        lastValue,
    }
}
