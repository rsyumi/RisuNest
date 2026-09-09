<script lang="ts">
    import { onDestroy, onMount, untrack } from 'svelte'
    import type { StreamingDisplayOptimizationMode } from 'src/ts/storage/database.svelte'
    import { chatMountProbe } from './chatMountProbe'
    import type { BoundedLiveChatParserProjection } from 'src/ts/selectedConversationLiveParserProjection'
    import type { ChatDisplayRefresh } from 'src/ts/chatDisplayRefresh'

    let {
        message,
        idx,
        img,
        character,
        rawStreamingText,
        bookmarked = false,
        parserProjection,
        parserAbortSignal,
    }: {
        message: string
        idx: number
        img: string
        character: unknown
        rawStreamingText: string
        bookmarked?: boolean
        parserProjection?: BoundedLiveChatParserProjection
        parserAbortSignal?: AbortSignal
    } = $props()

    const instanceId = chatMountProbe.nextInstanceId++
    if (untrack(() => idx) >= 0 && chatMountProbe.throwNextMount) {
        chatMountProbe.throwNextMount = false
        throw new Error('chat mount probe failure')
    }
    let displayedStreamingText = $state('')
    let refreshCount = $state(0)
    let optimizedStreaming = $state(true)

    export function hasStreamingPreview() {
        return optimizedStreaming && displayedStreamingText.includes('<Thoughts>')
    }

    export function updateStreamingDisplay(state: {
        isOptimizedStreamingMessage: boolean
        streamingOptimizationMode: StreamingDisplayOptimizationMode
        rawStreamingText: string
    }) {
        displayedStreamingText = state.rawStreamingText
        optimizedStreaming = state.isOptimizedStreamingMessage
        chatMountProbe.streamingUpdates.push({
            instanceId,
            rawStreamingText: state.rawStreamingText,
            isOptimizedStreamingMessage: state.isOptimizedStreamingMessage,
        })
    }

    export function updateViewportBinding() {}
    export function refreshMessageDisplay(state: ChatDisplayRefresh) {
        message = state.message
        parserProjection = state.parserProjection
        parserAbortSignal = state.parserAbortSignal
        refreshCount = untrack(() => refreshCount) + 1
        chatMountProbe.displayUpdates.push({
            instanceId,
            index: idx,
            message,
            signal: parserAbortSignal,
        })
    }

    export function refreshParserProjection(
        projection?: BoundedLiveChatParserProjection,
    ) {
        parserProjection = projection
        refreshCount = untrack(() => refreshCount) + 1
    }

    onMount(() => {
        displayedStreamingText = rawStreamingText
        chatMountProbe.mounts.push({
            instanceId,
            message,
            index: idx,
            image: img,
            character,
            bookmarked,
            parserProjectionKind: parserProjection?.kind,
            projectedChatID: parserProjection?.projectedChatID,
            parserAbortSignal,
        })
    })

    onDestroy(() => {
        chatMountProbe.unmounts.push(instanceId)
    })
</script>

<div
    data-chat-probe={instanceId}
    data-message={message}
    data-index={idx}
    data-image={img}
    data-streaming-text={displayedStreamingText}
    data-bookmarked={bookmarked}
    data-refresh-count={refreshCount}
></div>
